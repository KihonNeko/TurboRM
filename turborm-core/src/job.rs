use anyhow::{anyhow, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;

pub const MAX_PDF: usize = 100 * 1024 * 1024;
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Part {
    pub path: String,
    pub size: u64,
    pub hash: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Prepared,
    Sending,
    Completed,
    Verified,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    pub version: u32,
    pub uuid: Uuid,
    pub entry_hash: String,
    pub files: Vec<Part>,
    pub state: State,
    pub completed_generation: Option<u64>,
    #[serde(default)]
    pub verified_library_generation: Option<u64>,
    #[serde(default)]
    pub hash_tree: Option<TreeUpload>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TreeUpload {
    pub old_root_hash: String,
    pub old_generation: u64,
    pub document_hash: String,
    pub new_root_hash: String,
}
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
pub fn hash(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}
pub fn aggregate(hashes: impl Iterator<Item = [u8; 32]>) -> String {
    let mut sums = [0u64; 4];
    for h in hashes {
        for (i, sum) in sums.iter_mut().enumerate() {
            *sum = sum.wrapping_add(u64::from_be_bytes(h[i * 8..i * 8 + 8].try_into().unwrap()));
        }
    }
    hex(&sums
        .iter()
        .flat_map(|n| n.to_be_bytes())
        .collect::<Vec<_>>())
}
fn digest(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}
pub fn read_bounded(path: &Path, max: usize) -> Result<Vec<u8>> {
    let f = File::open(path).context("Cannot open local file")?;
    ensure!(f.metadata()?.is_file(), "Input must be a regular file");
    let mut bytes = Vec::new();
    f.take(max as u64 + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= max, "File exceeds application size limit");
    Ok(bytes)
}
fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut f = OpenOptions::new().write(true).create_new(true).open(path)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    Ok(())
}
pub fn prepare(pdf: &Path, output: &Path, title: Option<&str>) -> Result<Job> {
    prepare_pdf(pdf, output, title, "")
}
/// Pages at least twice as tall as they are wide are long pages, made to be
/// scrolled rather than seen whole.
const LONG_PAGE_RATIO: f32 = 2.0;

/// True when the PDF's first page is a long page. The `/MediaBox` may be
/// inherited from the page tree; `/Rotate` 90 or 270 swaps the sides.
fn first_page_is_long(document: &lopdf::Document) -> bool {
    let Some(&page) = document.get_pages().values().next() else {
        return false;
    };
    let inherited = |key: &[u8]| {
        let mut id = page;
        for _ in 0..64 {
            let dictionary = document.get_dictionary(id).ok()?;
            if let Ok(value) = dictionary.get(key) {
                return document.dereference(value).ok().map(|(_, value)| value);
            }
            id = dictionary.get(b"Parent").ok()?.as_reference().ok()?;
        }
        None
    };
    let Some(Ok(media)) = inherited(b"MediaBox").map(lopdf::Object::as_array) else {
        return false;
    };
    let numbers: Vec<f32> = media.iter().filter_map(|n| n.as_float().ok()).collect();
    let [x0, y0, x1, y1] = numbers[..] else {
        return false;
    };
    let (mut width, mut height) = ((x1 - x0).abs(), (y1 - y0).abs());
    let rotate = inherited(b"Rotate")
        .and_then(|r| r.as_i64().ok())
        .unwrap_or(0);
    if rotate.rem_euclid(180) == 90 {
        std::mem::swap(&mut width, &mut height);
    }
    width > 0.0 && height >= width * LONG_PAGE_RATIO
}
pub(crate) fn prepare_pdf(
    pdf: &Path,
    output: &Path,
    title: Option<&str>,
    parent: &str,
) -> Result<Job> {
    ensure!(
        !output.exists(),
        "Output job directory already exists; choose a new path"
    );
    let bytes = read_bounded(pdf, MAX_PDF)?;
    ensure!(bytes.starts_with(b"%PDF-"), "Input must be a PDF file");
    let document = lopdf::Document::load_mem(&bytes)
        .map_err(|_| anyhow!("Could not parse PDF page structure"))?;
    ensure!(
        !document.is_encrypted() && !document.trailer.has(b"Encrypt"),
        "Encrypted PDFs are not supported"
    );
    let count = document.get_pages().len();
    ensure!(
        (1..=10000).contains(&count),
        "PDF must contain 1 to 10,000 pages"
    );
    let title = title
        .or_else(|| pdf.file_stem().and_then(|s| s.to_str()))
        .ok_or_else(|| anyhow!("Supply a document title"))?;
    ensure!(
        !title.trim().is_empty()
            && title.chars().count() <= 250
            && !title.chars().any(char::is_control),
        "Title must contain 1 to 250 characters without control characters"
    );
    let id = Uuid::new_v4();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_millis()
        .to_string();
    let pages: Vec<String> = (0..count).map(|_| Uuid::new_v4().to_string()).collect();
    // Firmware's legacy PDF representation avoids speculative v2 position allocation.
    // PDF-IMPORT-CONTRACT.md records the evidence and remaining device acceptance test.
    // A long scrolling page shown whole would be a thin strip on the screen,
    // so the tablet opens it fitted to the width instead.
    let zoom = if first_page_is_long(&document) {
        "fitToWidth"
    } else {
        "bestFit"
    };
    let content = json!({"formatVersion":1,"fileType":"pdf","pageCount":count,
        "originalPageCount":count,"pages":pages,"redirectionPageMap":(0..count).collect::<Vec<_>>(),
        "coverPageNumber":0,"textScale":1,"lineHeight":100,"fontName":"",
        "textAlignment":"justify","orientation":"portrait","zoomMode":zoom,
        "customZoomScale":1,"customZoomCenterX":0,"customZoomCenterY":936,
        "customZoomPageWidth":1404,"customZoomPageHeight":1872,"customZoomOrientation":"portrait",
        "extraMetadata":{},"documentMetadata":{},"tags":[],"pageTags":[],
        "sizeInBytes":bytes.len().to_string()});
    let metadata = json!({"type":"DocumentType","visibleName":title,"parent":parent,
        "lastModified":now,"createdTime":now,"lastOpened":"0","lastOpenedPage":-1,
        "pinned":false,"new":false,"source":""});
    let components = vec![
        ("pdf", bytes),
        ("content", serde_json::to_vec_pretty(&content)?),
        ("metadata", serde_json::to_vec_pretty(&metadata)?),
        ("pagedata", "Blank\n".repeat(count).into_bytes()),
    ];
    create_private_dir(output)?;
    let files = components
        .iter()
        .map(|(extension, bytes)| Part {
            path: format!("{id}.{extension}"),
            size: bytes.len() as u64,
            hash: hash(bytes),
        })
        .collect::<Vec<_>>();
    for (part, (_, bytes)) in files.iter().zip(&components) {
        write_new(&output.join(&part.path), bytes)?;
    }
    let job = Job {
        version: 1,
        uuid: id,
        entry_hash: aggregate(components.iter().map(|(_, b)| digest(b))),
        files,
        state: State::Prepared,
        completed_generation: None,
        verified_library_generation: None,
        hash_tree: None,
    };
    job.save(output)?;
    Ok(job)
}
impl Job {
    pub fn load(dir: &Path) -> Result<Self> {
        let job: Self = serde_json::from_slice(&read_bounded(&dir.join("job.json"), 65536)?)
            .map_err(|_| anyhow!("Invalid upload job manifest"))?;
        ensure!(
            job.version == 1 && !job.uuid.is_nil() && crate::cloud::is_hash(&job.entry_hash),
            "Invalid job identity"
        );
        ensure!(
            job.files.len() == 4,
            "Expected four document bundle components"
        );
        let has_pdf = job
            .files
            .iter()
            .any(|f| f.path == format!("{}.pdf", job.uuid));
        let has_epub = job
            .files
            .iter()
            .any(|f| f.path == format!("{}.epub", job.uuid));
        ensure!(
            has_pdf ^ has_epub,
            "Expected exactly one PDF or EPUB original"
        );
        let original = if has_pdf { "pdf" } else { "epub" };
        for extension in [original, "content", "metadata", "pagedata"] {
            let path = format!("{}.{extension}", job.uuid);
            ensure!(
                job.files.iter().filter(|p| p.path == path).count() == 1,
                "Unexpected component paths"
            );
        }
        ensure!(
            job.files
                .iter()
                .all(|f| crate::cloud::is_hash(&f.hash) && f.size <= MAX_PDF as u64),
            "Invalid component descriptor"
        );
        if let Some(tree) = &job.hash_tree {
            ensure!(
                [
                    &tree.old_root_hash,
                    &tree.document_hash,
                    &tree.new_root_hash
                ]
                .iter()
                .all(|h| crate::cloud::is_hash(h))
                    && tree.old_generation <= i64::MAX as u64,
                "Invalid hash-tree journal"
            );
        }
        Ok(job)
    }
    pub fn save(&self, dir: &Path) -> Result<()> {
        let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
        serde_json::to_writer_pretty(&mut tmp, self)?;
        tmp.write_all(b"\n")?;
        tmp.as_file().sync_all()?;
        tmp.persist(dir.join("job.json"))
            .map_err(|_| anyhow!("Cannot persist upload journal"))?;
        Ok(())
    }
    pub fn payloads(&self, dir: &Path) -> Result<Vec<Vec<u8>>> {
        let mut result = Vec::new();
        for f in &self.files {
            let bytes = read_bounded(&dir.join(&f.path), MAX_PDF)?;
            ensure!(
                bytes.len() as u64 == f.size && hash(&bytes) == f.hash,
                "Prepared component changed; refusing upload"
            );
            result.push(bytes);
        }
        ensure!(
            aggregate(result.iter().map(|b| digest(b))) == self.entry_hash,
            "Bundle checksum does not match manifest"
        );
        Ok(result)
    }
}
// Held open for the full send/verify operation. A crash releases the OS lock.
// The durable Sending state independently prevents replay after a crash.
pub fn lock(dir: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(dir.join("operation.lock"))?;
    file.try_lock()
        .map_err(|_| anyhow!("Another operation is using this upload job"))?;
    Ok(file)
}
pub fn default_job_dir() -> Result<PathBuf> {
    let base = if cfg!(target_os = "linux") {
        std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .or_else(|| {
                std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state"))
            })
            .ok_or_else(|| anyhow!("XDG_STATE_HOME and HOME are unavailable; use --out"))?
    } else {
        PathBuf::from(
            std::env::var_os("LOCALAPPDATA")
                .ok_or_else(|| anyhow!("LOCALAPPDATA is unavailable; use --out"))?,
        )
    };
    Ok(base
        .join("remarkable-send")
        .join("jobs")
        .join(Uuid::new_v4().to_string()))
}

pub(crate) fn create_private_dir(output: &Path) -> Result<()> {
    ensure!(
        !output.exists(),
        "Output job directory already exists; choose a new path"
    );
    fs::create_dir_all(
        output
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new(".")),
    )?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new().mode(0o700).create(output)?;
    }
    #[cfg(not(unix))]
    fs::create_dir(output)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    pub fn pdf_bytes() -> Vec<u8> {
        pdf_with_box(100, 100, 0)
    }
    fn pdf_with_box(width: i64, height: i64, rotate: i64) -> Vec<u8> {
        use lopdf::{dictionary, Object, Stream};
        let mut d = lopdf::Document::with_version("1.5");
        let pages = d.new_object_id();
        let stream = d.add_object(Stream::new(dictionary! {}, vec![]));
        let page = d.add_object(
            dictionary! {"Type"=>"Page", "Parent"=>pages, "Rotate"=>rotate, "Contents"=>stream},
        );
        // The box is inherited from the page tree, as many generators write it.
        d.objects.insert(
            pages,
            Object::Dictionary(
                dictionary! {"Type"=>"Pages","Kids"=>vec![page.into()],"Count"=>1,
                "MediaBox"=>vec![0.into(),0.into(),width.into(),height.into()]},
            ),
        );
        let catalog = d.add_object(dictionary! {"Type"=>"Catalog","Pages"=>pages});
        d.trailer.set("Root", catalog);
        let mut bytes = Vec::new();
        d.save_to(&mut bytes).unwrap();
        bytes
    }
    #[test]
    fn sums_wrap_independently() {
        let a = [255u8; 32];
        let mut b = [0u8; 32];
        b[7] = 1;
        let r = aggregate([a, b].into_iter());
        assert_eq!(r, format!("{}{}", "0".repeat(16), "f".repeat(48)));
    }
    #[test]
    fn prepare_preserves_pdf_and_detects_tampering() {
        let temp = tempfile::tempdir().unwrap();
        let input = temp.path().join("one.pdf");
        let original = pdf_bytes();
        fs::write(&input, &original).unwrap();
        let out = temp.path().join("job");
        let job = prepare(&input, &out, None).unwrap();
        assert_eq!(job.payloads(&out).unwrap()[0], original);
        let content: serde_json::Value =
            serde_json::from_slice(&job.payloads(&out).unwrap()[1]).unwrap();
        assert_eq!(content["pages"].as_array().unwrap().len(), 1);
        assert_eq!(content["redirectionPageMap"], json!([0]));
        assert!(prepare(&input, &out, None).is_err());
        fs::write(out.join(&job.files[0].path), b"changed").unwrap();
        assert!(job.payloads(&out).is_err());
    }
    #[test]
    fn long_pages_open_fitted_to_the_width() {
        let zoom = |bytes: Vec<u8>| {
            let temp = tempfile::tempdir().unwrap();
            let input = temp.path().join("page.pdf");
            fs::write(&input, bytes).unwrap();
            let out = temp.path().join("job");
            let job = prepare(&input, &out, None).unwrap();
            let content: serde_json::Value =
                serde_json::from_slice(&job.payloads(&out).unwrap()[1]).unwrap();
            content["zoomMode"].as_str().unwrap().to_owned()
        };
        // 180 x 2482 mm, the shape of a long review page.
        assert_eq!(zoom(pdf_with_box(510, 7035, 0)), "fitToWidth");
        assert_eq!(zoom(pdf_with_box(510, 1020, 0)), "fitToWidth");
        // A4, a square page and a long page turned sideways keep the default.
        assert_eq!(zoom(pdf_with_box(595, 842, 0)), "bestFit");
        assert_eq!(zoom(pdf_bytes()), "bestFit");
        assert_eq!(zoom(pdf_with_box(510, 7035, 90)), "bestFit");
    }
    #[test]
    fn manifest_rejects_traversal_and_duplicate_files() {
        let temp = tempfile::tempdir().unwrap();
        let input = temp.path().join("one.pdf");
        fs::write(&input, pdf_bytes()).unwrap();
        let out = temp.path().join("job");
        let mut job = prepare(&input, &out, None).unwrap();
        job.files[0].path = "../secret".into();
        job.save(&out).unwrap();
        assert!(Job::load(&out).is_err());
    }
    #[test]
    fn concurrent_operations_are_locked() {
        let t = tempfile::tempdir().unwrap();
        let first = lock(t.path()).unwrap();
        assert!(lock(t.path()).is_err());
        drop(first);
        assert!(lock(t.path()).is_ok());
    }
}
