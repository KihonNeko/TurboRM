//! Offline construction of upload jobs from supported desktop document formats.
use anyhow::{anyhow, ensure, Context, Result};
use image::ImageReader;
use lopdf::{dictionary, Document, Object, Stream};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    fs::{self, OpenOptions},
    io::{Cursor, Read, Write},
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;

use crate::job::{self, Job, Part, State, MAX_PDF};

const MAX_IMAGE_PIXELS: u64 = 40_000_000;
const MAX_EPUB_EXPANDED: u64 = 500 * 1024 * 1024;
const MAX_EPUB_ENTRIES: usize = 10_000;
const MAX_EPUB_XML: u64 = 4 * 1024 * 1024;
const OPF_NAMESPACE: &str = "http://www.idpf.org/2007/opf";

/// Prepare a local, reviewable upload bundle. This function performs no network I/O.
pub fn prepare_import(
    input: &Path,
    output: &Path,
    title: Option<&str>,
    parent: &str,
) -> Result<Job> {
    validate_parent(parent)?;
    let extension = input
        .extension()
        .and_then(|value| value.to_str())
        .map(str::to_ascii_lowercase)
        .ok_or_else(|| anyhow!("Input must have a PDF, EPUB, PNG, JPG, or JPEG extension"))?;
    match extension.as_str() {
        "pdf" => job::prepare_pdf(input, output, title, parent),
        "epub" => prepare_epub(input, output, title, parent),
        "png" | "jpg" | "jpeg" => prepare_image(input, output, title, parent),
        _ => Err(anyhow!(
            "Supported imports are PDF, EPUB, PNG, JPG, and JPEG"
        )),
    }
}

fn validate_parent(parent: &str) -> Result<()> {
    if !parent.is_empty() {
        let parsed = Uuid::parse_str(parent).context("Parent must be empty or a canonical UUID")?;
        ensure!(
            !parsed.is_nil() && parsed.to_string() == parent,
            "Parent must be empty or a non-nil canonical UUID"
        );
    }
    Ok(())
}

fn title_for<'a>(input: &'a Path, title: Option<&'a str>) -> Result<&'a str> {
    let title = title
        .or_else(|| input.file_stem().and_then(|value| value.to_str()))
        .ok_or_else(|| anyhow!("Supply a document title"))?;
    ensure!(
        !title.trim().is_empty()
            && title.chars().count() <= 250
            && !title.chars().any(char::is_control),
        "Title must contain 1 to 250 characters without control characters"
    );
    Ok(title)
}

fn prepare_epub(input: &Path, output: &Path, title: Option<&str>, parent: &str) -> Result<Job> {
    let bytes = job::read_bounded(input, MAX_PDF)?;
    validate_epub(&bytes)?;
    let content = json!({
        "formatVersion": 1, "fileType": "epub", "pageCount": 0,
        "originalPageCount": 0, "pages": [], "redirectionPageMap": [],
        "coverPageNumber": 0, "textScale": 1, "lineHeight": 100,
        "fontName": "", "textAlignment": "justify", "orientation": "portrait",
        "extraMetadata": {}, "documentMetadata": {}, "tags": [], "pageTags": [],
        "sizeInBytes": bytes.len().to_string()
    });
    prepare_bundle(
        output,
        title_for(input, title)?,
        parent,
        "epub",
        bytes,
        serde_json::to_vec_pretty(&content)?,
        Vec::new(),
    )
}

fn validate_epub(bytes: &[u8]) -> Result<()> {
    let mut archive =
        zip::ZipArchive::new(Cursor::new(bytes)).context("Invalid EPUB ZIP container")?;
    ensure!(
        archive.len() <= MAX_EPUB_ENTRIES,
        "EPUB contains too many entries"
    );
    {
        let first = archive.by_index(0).context("EPUB is empty")?;
        ensure!(
            first.name() == "mimetype" && first.compression() == zip::CompressionMethod::Stored,
            "EPUB mimetype must be the first, uncompressed entry"
        );
    }
    let mut expanded = 0u64;
    let mut mimetypes = 0usize;
    let mut containers = 0usize;
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index)?;
        ensure!(
            entry.enclosed_name().is_some() && !entry.name().contains(['\\', ':']),
            "EPUB contains an unsafe path"
        );
        ensure!(!entry.encrypted(), "Encrypted EPUB entries are unsupported");
        // Do not trust the central-directory size. Reading each member to EOF
        // makes the ZIP reader verify its decompressed length and CRC too.
        let remaining = MAX_EPUB_EXPANDED
            .checked_sub(expanded)
            .ok_or_else(|| anyhow!("EPUB expanded size overflow"))?;
        let actual = std::io::copy(
            &mut entry.by_ref().take(remaining + 1),
            &mut std::io::sink(),
        )
        .context("Invalid or corrupt EPUB entry")?;
        expanded = expanded
            .checked_add(actual)
            .ok_or_else(|| anyhow!("EPUB expanded size overflow"))?;
        ensure!(
            expanded <= MAX_EPUB_EXPANDED,
            "EPUB expands beyond the application limit"
        );
        mimetypes += usize::from(entry.name() == "mimetype");
        containers += usize::from(entry.name() == "META-INF/container.xml");
    }
    ensure!(
        mimetypes == 1,
        "EPUB must contain exactly one mimetype entry"
    );
    ensure!(
        containers == 1,
        "EPUB must contain exactly one container descriptor"
    );
    let mut mimetype = String::new();
    archive
        .by_name("mimetype")
        .context("EPUB has no mimetype entry")?
        .take(64)
        .read_to_string(&mut mimetype)?;
    ensure!(mimetype == "application/epub+zip", "Invalid EPUB mimetype");
    let package_path = {
        let mut xml = String::new();
        archive
            .by_name("META-INF/container.xml")
            .context("EPUB has no container descriptor")?
            .take(1024 * 1024 + 1)
            .read_to_string(&mut xml)?;
        ensure!(
            xml.len() <= 1024 * 1024,
            "EPUB container descriptor is too large"
        );
        let document = roxmltree::Document::parse(&xml).context("Invalid EPUB container XML")?;
        document
            .descendants()
            .find(|node| node.has_tag_name("rootfile"))
            .and_then(|node| node.attribute("full-path"))
            .filter(|path| !path.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| anyhow!("EPUB container has no package path"))?
    };
    ensure!(
        Path::new(&package_path).is_relative()
            && !package_path.split('/').any(|component| component == ".."),
        "EPUB package path is unsafe"
    );
    let mut package_xml = String::new();
    archive
        .by_name(&package_path)
        .context("EPUB package document is missing")?
        .take(MAX_EPUB_XML + 1)
        .read_to_string(&mut package_xml)
        .context("EPUB package document is not UTF-8 XML")?;
    ensure!(
        package_xml.len() as u64 <= MAX_EPUB_XML,
        "EPUB package document is too large"
    );
    let package = roxmltree::Document::parse(&package_xml).context("Invalid EPUB package XML")?;
    let root = package.root_element();
    ensure!(
        root.tag_name().name() == "package" && root.tag_name().namespace() == Some(OPF_NAMESPACE),
        "EPUB package has an invalid root element or namespace"
    );
    Ok(())
}

fn prepare_image(input: &Path, output: &Path, title: Option<&str>, parent: &str) -> Result<Job> {
    let bytes = job::read_bounded(input, MAX_PDF)?;
    let reader = ImageReader::new(Cursor::new(&bytes))
        .with_guessed_format()
        .context("Cannot identify image")?;
    ensure!(
        matches!(
            reader.format(),
            Some(image::ImageFormat::Png | image::ImageFormat::Jpeg)
        ),
        "Image data must be PNG or JPEG"
    );
    let (width, height) = reader
        .into_dimensions()
        .context("Cannot read image dimensions")?;
    ensure!(width > 0 && height > 0, "Image dimensions must be non-zero");
    ensure!(
        u64::from(width) * u64::from(height) <= MAX_IMAGE_PIXELS,
        "Image exceeds the 40 megapixel limit"
    );
    let reader = ImageReader::new(Cursor::new(&bytes))
        .with_guessed_format()
        .context("Cannot identify image")?;
    let image = reader.decode().context("Cannot decode image")?;
    let pdf = image_pdf(image)?;
    ensure!(
        pdf.len() <= MAX_PDF,
        "Converted PDF exceeds the application size limit"
    );
    let temporary = tempfile::NamedTempFile::new().context("Cannot create converted PDF")?;
    fs::write(temporary.path(), pdf)?;
    job::prepare_pdf(
        temporary.path(),
        output,
        title.or_else(|| input.file_stem().and_then(|s| s.to_str())),
        parent,
    )
}

fn image_pdf(image: image::DynamicImage) -> Result<Vec<u8>> {
    // Paper is white: discarding alpha would turn transparent black pixels
    // into a black background instead of the image the user sees.
    let mut rgba = image.to_rgba8();
    for pixel in rgba.pixels_mut() {
        let alpha = u32::from(pixel[3]);
        for channel in &mut pixel.0[..3] {
            *channel = ((u32::from(*channel) * alpha + 255 * (255 - alpha) + 127) / 255) as u8;
        }
        pixel[3] = 255;
    }
    let rgb = image::DynamicImage::ImageRgba8(rgba).to_rgb8();
    let (width, height) = rgb.dimensions();
    let mut document = Document::with_version("1.5");
    let pages = document.new_object_id();
    let mut picture = Stream::new(
        dictionary! {
            "Type" => "XObject", "Subtype" => "Image", "Width" => width as i64,
            "Height" => height as i64, "ColorSpace" => "DeviceRGB", "BitsPerComponent" => 8,
        },
        rgb.into_raw(),
    );
    picture.compress()?;
    let picture_id = document.add_object(picture);
    let page_width = width as f32 * 0.5;
    let page_height = height as f32 * 0.5;
    let commands = format!("q {page_width} 0 0 {page_height} 0 0 cm /Im0 Do Q");
    let commands_id = document.add_object(Stream::new(dictionary! {}, commands.into_bytes()));
    let page = document.add_object(dictionary! {
        "Type" => "Page", "Parent" => pages,
        "MediaBox" => vec![0.into(), 0.into(), page_width.into(), page_height.into()],
        "Resources" => dictionary! { "XObject" => dictionary! { "Im0" => picture_id } },
        "Contents" => commands_id,
    });
    document.objects.insert(
        pages,
        Object::Dictionary(dictionary! {
            "Type" => "Pages", "Kids" => vec![page.into()], "Count" => 1,
        }),
    );
    let catalog = document.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages });
    document.trailer.set("Root", catalog);
    let mut bytes = Vec::new();
    document.save_to(&mut bytes)?;
    Ok(bytes)
}

fn prepare_bundle(
    output: &Path,
    title: &str,
    parent: &str,
    extension: &str,
    original: Vec<u8>,
    content: Vec<u8>,
    pagedata: Vec<u8>,
) -> Result<Job> {
    job::create_private_dir(output)?;
    let id = Uuid::new_v4();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_millis()
        .to_string();
    let metadata = serde_json::to_vec_pretty(&json!({
        "type":"DocumentType", "visibleName":title, "parent":parent,
        "lastModified":now, "createdTime":now, "lastOpened":"0", "lastOpenedPage":-1,
        "pinned":false, "new":false, "source":""
    }))?;
    let components = vec![
        (extension, original),
        ("content", content),
        ("metadata", metadata),
        ("pagedata", pagedata),
    ];
    let files = components
        .iter()
        .map(|(ext, bytes)| Part {
            path: format!("{id}.{ext}"),
            size: bytes.len() as u64,
            hash: job::hash(bytes),
        })
        .collect::<Vec<_>>();
    for (part, (_, bytes)) in files.iter().zip(&components) {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(output.join(&part.path))?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    let entry_hash = job::aggregate(
        components
            .iter()
            .map(|(_, bytes)| Sha256::digest(bytes).into()),
    );
    let result = Job {
        version: 1,
        uuid: id,
        entry_hash,
        files,
        state: State::Prepared,
        completed_generation: None,
        verified_library_generation: None,
        hash_tree: None,
    };
    result.save(output)?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn transparent_image_pixels_are_composited_on_white_paper() {
        let rgba = image::RgbaImage::from_pixel(1, 1, image::Rgba([0, 0, 0, 0]));
        let bytes = image_pdf(image::DynamicImage::ImageRgba8(rgba)).unwrap();
        let pdf = Document::load_mem(&bytes).unwrap();
        let stream = pdf
            .objects
            .values()
            .filter_map(|object| object.as_stream().ok())
            .find(|stream| {
                stream
                    .dict
                    .get(b"Subtype")
                    .ok()
                    .and_then(|value| value.as_name().ok())
                    == Some(b"Image")
            })
            .unwrap();
        let pixels = stream
            .decompressed_content()
            .unwrap_or_else(|_| stream.content.clone());
        assert_eq!(pixels, [255, 255, 255]);
    }

    fn epub_with_opf(opf: &[u8]) -> Vec<u8> {
        let mut bytes = Cursor::new(Vec::new());
        let mut zip = zip::ZipWriter::new(&mut bytes);
        zip.start_file(
            "mimetype",
            zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored),
        )
        .unwrap();
        zip.write_all(b"application/epub+zip").unwrap();
        zip.start_file(
            "META-INF/container.xml",
            zip::write::SimpleFileOptions::default(),
        )
        .unwrap();
        zip.write_all(
            b"<container><rootfiles><rootfile full-path=\"book.opf\"/></rootfiles></container>",
        )
        .unwrap();
        zip.start_file(
            "book.opf",
            zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored),
        )
        .unwrap();
        zip.write_all(opf).unwrap();
        zip.start_file(
            "payload.bin",
            zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored),
        )
        .unwrap();
        zip.write_all(b"PAYLOAD-CHECKSUM-TARGET").unwrap();
        zip.finish().unwrap();
        bytes.into_inner()
    }

    fn epub_bytes() -> Vec<u8> {
        epub_with_opf(br#"<package xmlns="http://www.idpf.org/2007/opf"/>"#)
    }

    #[test]
    fn epub_is_preserved_and_parent_is_recorded() {
        let temp = tempfile::tempdir().unwrap();
        let input = temp.path().join("book.epub");
        let original = epub_bytes();
        fs::write(&input, &original).unwrap();
        let output = temp.path().join("job");
        let parent = Uuid::new_v4().to_string();
        let prepared = prepare_import(&input, &output, None, &parent).unwrap();
        assert_eq!(prepared.payloads(&output).unwrap()[0], original);
        let metadata: serde_json::Value =
            serde_json::from_slice(&prepared.payloads(&output).unwrap()[2]).unwrap();
        assert_eq!(metadata["parent"], parent);
        assert_eq!(Job::load(&output).unwrap().uuid, prepared.uuid);
    }

    #[test]
    fn rejects_noncanonical_parent_and_bad_epub() {
        let temp = tempfile::tempdir().unwrap();
        let input = temp.path().join("bad.epub");
        fs::write(&input, b"not a zip").unwrap();
        assert!(prepare_import(&input, &temp.path().join("one"), None, "NOT-A-UUID").is_err());
        assert!(prepare_import(&input, &temp.path().join("two"), None, "").is_err());
    }

    #[test]
    fn rejects_invalid_package_xml_and_corrupt_member() {
        let invalid_opf = epub_with_opf(b"<metadata/>");
        assert!(validate_epub(&invalid_opf).is_err());

        let mut corrupt = epub_bytes();
        let payload = b"PAYLOAD-CHECKSUM-TARGET";
        let offset = corrupt
            .windows(payload.len())
            .position(|window| window == payload)
            .unwrap();
        corrupt[offset] ^= 1;
        let error = validate_epub(&corrupt).unwrap_err().to_string();
        assert!(error.contains("corrupt EPUB entry"), "{error}");
    }

    #[test]
    fn png_becomes_single_page_pdf() {
        let temp = tempfile::tempdir().unwrap();
        let input = temp.path().join("pixel.png");
        image::DynamicImage::new_rgb8(2, 3).save(&input).unwrap();
        let output = temp.path().join("job");
        let prepared = prepare_import(&input, &output, Some("Pixel"), "").unwrap();
        assert!(prepared.payloads(&output).unwrap()[0].starts_with(b"%PDF-"));
        assert!(Job::load(&output).is_ok());
    }
}
