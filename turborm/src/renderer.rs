//! Local page rendering. No network or credentials are available in this module.

/// Shared PDF-background placement. Cover rendering, viewer tiles, SVG export
/// and the preview PDF all reach it through [`Document::svg`].
#[path = "pdf_geometry.rs"]
pub mod pdf_geometry;

use anyhow::{anyhow, bail, ensure, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use std::{
    collections::VecDeque,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use turborm_core::{
    job,
    notebook::{self, PageRef},
};

/// How many rendered pages the document keeps, so stepping back and forth
/// between neighbouring pages repaints from memory instead of re-rendering.
pub const PAGE_CACHE_LIMIT: usize = 4;

/// Whole-page rasters kept per page: the cheap first paint and one detail size.
const OVERVIEW_CACHE_LIMIT: usize = 2;

/// How much finer than the page's stored background a tile may need before it
/// renders its part of the PDF page again at its own resolution. Below this,
/// enlarging the stored raster is indistinguishable.
const SHARPEN_RATIO: f32 = 1.25;

/// The pixel size of one band of a PDF page rendered at a tile's resolution.
/// A band spans many tiles, so one Poppler run serves them all. Bands are
/// large because a Poppler run costs about the same whatever its size: it
/// decodes the page's whole scanned image each time. At 2x, 2048 x 512
/// bands took 10.6 s for one screen of a scanned textbook, these 1.7 s.
/// Its PPM output (24 MiB) stays under the sandbox's 32 MiB file limit.
const BAND_WIDTH: u32 = 4096;
const BAND_HEIGHT: u32 = 2048;

/// Bands kept in memory, at most 32 MiB each: about two pages at 2x.
const BAND_CACHE_LIMIT: usize = 4;

/// One band of a PDF background: the band grid of the whole page rendered
/// at `width` x `height` pixels.
#[derive(Clone, Debug, PartialEq, Eq)]
struct BandKey {
    page: usize,
    width: u32,
    height: u32,
    column: u32,
    row: u32,
}

type BandCell = Arc<std::sync::OnceLock<Result<Arc<resvg::tiny_skia::Pixmap>, String>>>;

/// A page more than this many times taller than wide is a long page, made to
/// be scrolled: its stills are sized by width rather than by height.
pub const LONG_PAGE_RATIO: f32 = 1.5;

/// The tallest still of a long page, in pixels.
pub const MAX_LONG_STILL: u32 = 60_000;

pub fn is_long_page(width: f32, height: f32) -> bool {
    width > 0.0 && height > width * LONG_PAGE_RATIO
}

/// The pixel size of a still of a `width` x `height` page for `longest_side`.
///
/// An ordinary page fits `longest_side`. A long page is as wide as a 3:4
/// page of that longest side, so its text is as legible as on an ordinary
/// page, and as tall as its shape needs, up to [`MAX_LONG_STILL`].
pub fn still_size(width: f32, height: f32, longest_side: u32) -> (u32, u32) {
    let pixels_wide = if is_long_page(width, height) {
        (longest_side as f32 * 0.75).min(MAX_LONG_STILL as f32 * width / height)
    } else {
        longest_side as f32 * width / width.max(height)
    };
    let scale = pixels_wide / width;
    (
        (width * scale).round().max(1.0) as u32,
        (height * scale).round().max(1.0) as u32,
    )
}

/// One whole-page raster, identified by the longest side it was rendered to.
struct PageRaster {
    longest_side: u32,
    rgba: Vec<u8>,
    width: usize,
    height: usize,
}

pub struct Document {
    // Own the private download for as long as the viewer is open.
    directory: tempfile::TempDir,
    id: String,
    pub title: String,
    pub pages: Vec<PageRef>,
    background_pdf: Option<PathBuf>,
    template_settings: serde_json::Value,
    pub warnings: Vec<String>,
    pub cover_page: usize,
    pub source_hash: Option<String>,
    document_key: String,
    landscape: bool,
    pdf_longest_side: u32,
    /// Set by [`Document::load_for_stills`]: the still size PDF pages are
    /// rasterized for, so a long page is rasterized by its width.
    still_side: Option<u32>,
    /// The last [`PAGE_CACHE_LIMIT`] rendered pages, most recent last. Shared
    /// so tile workers can render from a page while the foreground thread
    /// prepares the next one.
    rendered: Mutex<VecDeque<Arc<CachedPage>>>,
    // Page boxes come from the PDF itself, so parse it at most once.
    page_boxes: Mutex<Option<Vec<pdf_geometry::PdfPage>>>,
    /// Sharp PDF bands for zoomed-in and long pages, most recent last.
    bands: Mutex<VecDeque<(BandKey, BandCell)>>,
}

/// One page, ready to rasterize from any thread.
///
/// The PDF background is kept decoded rather than re-parsed out of the SVG for
/// every tile: `tree` holds the ink and template only, and `background` is
/// composited under it. `usvg::Tree` is `Sync`, so several tile workers can
/// share one `Arc<CachedPage>`.
pub struct CachedPage {
    index: usize,
    /// Template and ink. The PDF background is **not** an `<image>` here.
    tree: resvg::usvg::Tree,
    /// The decoded PDF background and the canvas rectangle it occupies.
    background: Option<(resvg::tiny_skia::Pixmap, notebook::CanvasBounds)>,
    /// The source PDF page of `background`, which a zoomed-in tile renders
    /// again at its own resolution (see [`Document::tile_from`]).
    source_page: Option<usize>,
    warnings: Vec<String>,
    /// Whole-page rasters already produced for this page, so returning to a
    /// page repaints without rasterizing again. Bounded at
    /// [`OVERVIEW_CACHE_LIMIT`] entries per page.
    overview: Mutex<Vec<PageRaster>>,
}

impl CachedPage {
    pub fn index(&self) -> usize {
        self.index
    }

    pub fn size(&self) -> resvg::usvg::Size {
        self.tree.size()
    }

    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// Paint the background region and the ink into `pixmap`, where the
    /// transform maps canvas units to `pixmap` pixels.
    fn paint(&self, transform: resvg::tiny_skia::Transform, pixmap: &mut resvg::tiny_skia::Pixmap) {
        pixmap.fill(resvg::tiny_skia::Color::WHITE);
        if let Some((background, rect)) = &self.background {
            // Fill exactly the background rectangle with the decoded raster as
            // a pattern. This is what resvg does for the `<image>` element the
            // exported SVG still carries, minus the per-tile PNG decode: the
            // rect is antialiased at its fractional edges and the pattern
            // transform maps raster pixels onto that rectangle.
            let to_rect = resvg::tiny_skia::Transform::from_translate(rect.x, rect.y).pre_scale(
                rect.width / background.width().max(1) as f32,
                rect.height / background.height().max(1) as f32,
            );
            if let Some(area) = resvg::tiny_skia::Rect::from_xywh(
                rect.x,
                rect.y,
                rect.width.max(f32::EPSILON),
                rect.height.max(f32::EPSILON),
            ) {
                let paint = resvg::tiny_skia::Paint {
                    shader: resvg::tiny_skia::Pattern::new(
                        background.as_ref(),
                        resvg::tiny_skia::SpreadMode::Pad,
                        // resvg's default `image-rendering: optimizeQuality`.
                        resvg::tiny_skia::FilterQuality::Bicubic,
                        1.0,
                        to_rect,
                    ),
                    ..Default::default()
                };
                pixmap.fill_rect(area, &paint, transform, None);
            }
        }
        resvg::render(&self.tree, transform, &mut pixmap.as_mut());
    }
}

pub const TILE_SIZE: usize = 512;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TileKey {
    pub document_key: String,
    pub page: usize,
    pub scale_milli: u32,
    pub column: u32,
    pub row: u32,
}

pub struct Tile {
    pub key: TileKey,
    pub rgba: Vec<u8>,
    pub width: usize,
    pub height: usize,
}

pub struct Preview {
    pub rgba: Vec<u8>,
    pub width: usize,
    pub height: usize,
    pub page: usize,
    pub total: usize,
    pub title: String,
    pub warnings: Vec<String>,
    pub document_key: String,
    pub canvas_width: f32,
    pub canvas_height: f32,
}

/// Largest decoded image the raster path re-encodes, and the most it
/// re-encodes for one page. Larger images keep their original encoding.
const RASTER_IMAGE_PIXELS_BYTES: usize = 96 * 1024 * 1024;
const RASTER_PAGE_PIXELS_BYTES: usize = 256 * 1024 * 1024;

/// Re-encode a page's embedded JPEG images as uncompressed PNG for the raster
/// path.
///
/// resvg decodes an `<image>` every time it renders and does not skip one
/// outside the tile, so every tile of a page would decode every JPEG on it
/// again. An uncompressed PNG decodes an order of magnitude faster than a
/// progressive JPEG. Only the viewer's cached page uses this; SVG and PDF
/// exports keep the original files. An image that cannot be re-encoded within
/// the bounds keeps its original encoding.
fn raster_friendly_images(svg: &str) -> String {
    const PREFIX: &str = "xlink:href=\"data:image/jpeg;base64,";
    let mut out = String::with_capacity(svg.len());
    let mut rest = svg;
    let mut budget = RASTER_PAGE_PIXELS_BYTES;
    while let Some(start) = rest.find(PREFIX) {
        let data_start = start + PREFIX.len();
        let Some(length) = rest[data_start..].find('"') else {
            break;
        };
        let encoded = &rest[data_start..data_start + length];
        out.push_str(&rest[..start]);
        match transcode_jpeg(encoded, &mut budget) {
            Some(png) => {
                out.push_str("xlink:href=\"data:image/png;base64,");
                out.push_str(&png);
            }
            None => {
                out.push_str(PREFIX);
                out.push_str(encoded);
            }
        }
        rest = &rest[data_start + length..];
    }
    out.push_str(rest);
    out
}

fn transcode_jpeg(encoded: &str, budget: &mut usize) -> Option<String> {
    use image::ImageEncoder;
    let bytes = STANDARD.decode(encoded).ok()?;
    let mut reader =
        image::ImageReader::with_format(std::io::Cursor::new(&bytes), image::ImageFormat::Jpeg);
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(RASTER_IMAGE_PIXELS_BYTES as u64);
    reader.limits(limits);
    let decoded = reader.decode().ok()?.to_rgba8();
    let size = decoded.as_raw().len();
    if size > RASTER_IMAGE_PIXELS_BYTES || size > *budget {
        return None;
    }
    *budget -= size;
    let mut png = Vec::with_capacity(size + size / 64 + 1024);
    image::codecs::png::PngEncoder::new_with_quality(
        &mut png,
        image::codecs::png::CompressionType::Uncompressed,
        image::codecs::png::FilterType::NoFilter,
    )
    .write_image(
        decoded.as_raw(),
        decoded.width(),
        decoded.height(),
        image::ExtendedColorType::Rgba8,
    )
    .ok()?;
    Some(STANDARD.encode(png))
}

/// Remove `<text>` elements from a rendered page.
///
/// Used only when no system font is available, so that a page still shows its
/// ink instead of failing. The caller records a warning.
fn strip_text_elements(svg: &str) -> String {
    let mut out = String::with_capacity(svg.len());
    let mut rest = svg;
    while let Some(start) = rest.find("<text") {
        out.push_str(&rest[..start]);
        match rest[start..].find("</text>") {
            Some(end) => rest = &rest[start + end + "</text>".len()..],
            None => return out,
        }
    }
    out.push_str(rest);
    out
}

impl Document {
    pub fn load(directory: tempfile::TempDir, id: String, title: String) -> Result<Self> {
        let bundle = directory.path().join("bundle");
        let content: serde_json::Value = serde_json::from_slice(&job::read_bounded(
            &bundle.join(format!("{id}.content")),
            16 * 1024 * 1024,
        )?)?;
        let background_pdf = bundle.join(format!("{id}.pdf"));
        let background_pdf = background_pdf.is_file().then_some(background_pdf);
        // The active page map is authoritative. Falling back to every source
        // PDF page here would resurrect deleted/reordered notebook pages.
        let pages = notebook::pages(&content)?;
        ensure!(
            !pages.is_empty(),
            "No renderable pages yet. For a new EPUB, let the tablet sync and generate its pages, then refresh."
        );
        ensure!(
            pages.len() <= 10_000,
            "Document exceeds the 10,000-page limit"
        );
        let mut warnings = Vec::new();
        if content["fileType"] == "epub" && background_pdf.is_none() {
            bail!(
                "This EPUB has no cloud-generated PDF yet. Let the tablet open it and sync, then refresh. Original EPUB export is available."
            );
        }
        if content["zoomMode"].as_str().is_some_and(|z| z != "bestFit") {
            warnings.push("Custom page cropping is not reproduced in this preview.".into());
        }
        let metadata = job::read_bounded(&bundle.join(format!("{id}.metadata")), 16 * 1024 * 1024)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or(serde_json::Value::Null);
        let cover_page = turborm_core::cover::page_index(&content, &metadata, &pages).unwrap_or(0);
        // A new local instance has its own tile namespace, even if the same
        // document is reopened after a cloud update.
        let document_key = job::hash(directory.path().as_os_str().as_encoded_bytes());
        let landscape = content["orientation"] == "landscape";
        let mut template_settings = content;
        if template_settings["formatVersion"] == 1 {
            let pagedata = bundle.join(format!("{id}.pagedata"));
            let names = if pagedata.is_file() {
                String::from_utf8(job::read_bounded(&pagedata, 4 * 1024 * 1024)?)?
                    .lines()
                    .map(str::trim)
                    .filter(|name| !name.is_empty())
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            } else {
                Vec::new()
            };
            template_settings["cPages"] = serde_json::json!({"pages": pages.iter().enumerate().map(|(index, page)| {
                serde_json::json!({"id":page.id,"template":{"value":names.get(index).map_or("Blank", String::as_str)}})
            }).collect::<Vec<_>>()});
        }
        Ok(Self {
            directory,
            id,
            title,
            pages,
            background_pdf,
            template_settings,
            warnings,
            cover_page,
            source_hash: None,
            document_key,
            landscape,
            pdf_longest_side: 1800,
            still_side: None,
            rendered: Mutex::new(VecDeque::new()),
            page_boxes: Mutex::new(None),
            bands: Mutex::new(VecDeque::new()),
        })
    }

    pub fn load_for_cover(directory: tempfile::TempDir, id: String, title: String) -> Result<Self> {
        let mut document = Self::load(directory, id, title)?;
        // Library covers never need a full viewer-resolution PDF raster.
        document.pdf_longest_side = 280;
        Ok(document)
    }

    /// Load a document whose pages are rendered as [`Document::still`]s of up
    /// to `longest_side` pixels, rasterizing PDF backgrounds at that size
    /// instead of the viewer's 1800. The Poppler sandbox is unchanged.
    pub fn load_for_stills(
        directory: tempfile::TempDir,
        id: String,
        title: String,
        longest_side: u32,
    ) -> Result<Self> {
        ensure!(
            (1..=Self::MAX_STILL).contains(&longest_side),
            "Image size must be 1 to {} pixels",
            Self::MAX_STILL
        );
        let mut document = Self::load(directory, id, title)?;
        document.pdf_longest_side = longest_side;
        document.still_side = Some(longest_side);
        Ok(document)
    }

    /// The exported page: one SVG string with the PDF background embedded as a
    /// base64 `<image>`. SVG export and the preview PDF both go through here,
    /// so their output is unchanged by the viewer's faster raster path.
    pub fn svg(&self, index: usize) -> Result<(String, Vec<String>)> {
        let composed = self.compose(index, true)?;
        Ok((composed.svg, composed.warnings))
    }

    /// Build a page's SVG, optionally leaving the PDF background out of it.
    ///
    /// `embed_background: false` returns the template and ink only, with the
    /// decoded background handed back separately so the raster path can
    /// composite it once per page instead of re-decoding it per tile.
    fn compose(&self, index: usize, embed_background: bool) -> Result<Composed> {
        self.compose_layers(index, embed_background, Layers::Everything)
    }

    /// [`Document::compose`] with a choice of layers. [`Layers::Annotations`]
    /// leaves out the PDF background and the paper template but keeps the
    /// background's placement in the canvas bounds, so the result has exactly
    /// the geometry of the complete page.
    fn compose_layers(
        &self,
        index: usize,
        embed_background: bool,
        layers: Layers,
    ) -> Result<Composed> {
        let paper = layers == Layers::Everything;
        let page = self
            .pages
            .get(index)
            .ok_or_else(|| anyhow!("Page is out of range"))?;
        let mut warnings = self.warnings.clone();
        let mut background = None;
        let placed_pdf_page = self.background_pdf.as_ref().and(page.background);
        if let (Some(pdf), Some(number), true) = (&self.background_pdf, page.background, paper) {
            // While poppler has this PDF open, also raster the next page's
            // background when it is the immediately following source page.
            // One parse serves the page being shown and the likely next one.
            let also = self
                .pages
                .get(index + 1)
                .and_then(|next| next.background)
                .filter(|next| *next == number + 1);
            background = Some(std::thread::scope(|scope| {
                // Placing the background needs the page sizes, which means
                // parsing the whole PDF once; do that while Poppler rasterizes.
                if self.page_boxes.lock().is_ok_and(|boxes| boxes.is_none()) {
                    scope.spawn(|| {
                        let _ = self.page_box(number);
                    });
                }
                self.pdf_page_with(pdf, number, also)
            })?);
        }
        let scene = self
            .directory
            .path()
            .join("bundle")
            .join(&self.id)
            .join(format!("{}.rm", page.id));
        let scene_bytes = if !page.id.is_empty() && scene.is_file() {
            Some(job::read_bounded(&scene, 64 * 1024 * 1024)?)
        } else {
            None
        };
        let geometry = scene_bytes
            .as_deref()
            .map(notebook::scene_geometry)
            .transpose()?
            .unwrap_or_default();
        // A scene's nominal paper size and its PDF placement are distinct. The
        // paper only fixes the ink origin; the background keeps its own
        // physical size. Older files without scene geometry use the document
        // default, which follows the stored orientation.
        let fallback_size = if self.landscape {
            (pdf_geometry::PAPER_HEIGHT, pdf_geometry::PAPER_WIDTH)
        } else {
            (pdf_geometry::PAPER_WIDTH, pdf_geometry::PAPER_HEIGHT)
        };
        let (base_width, base_height) = geometry.paper_size.unwrap_or(fallback_size);
        let mut placement = None;
        if background.is_some() {
            // An unreadable page box means the background cannot be registered
            // against the ink, so drop it rather than draw it in the wrong place.
            match page
                .background
                .ok_or_else(|| anyhow!("Page has no source PDF page"))
                .and_then(|number| self.page_box(number))
                .and_then(|source| pdf_geometry::place(source, (base_width, base_height)))
            {
                Ok(placed) => placement = Some(placed.background),
                Err(error) => {
                    warnings.push(format!("PDF background not placed: {error:#}"));
                    background = None;
                }
            }
        } else if let (Some(number), false) = (placed_pdf_page, paper) {
            // The annotations alone still occupy the canvas the background
            // would: the same box, the same placement, but nothing rasterized.
            // A box the complete page could not place is left out here too.
            if let Ok(placed) = self
                .page_box(number)
                .and_then(|source| pdf_geometry::place(source, (base_width, base_height)))
            {
                placement = Some(placed.background);
            }
        }
        let ink = if let Some(bytes) = scene_bytes.as_deref() {
            // Inserted images live beside the scene in `<document>/<page>/`,
            // named by the scene's image table. A file that was not
            // downloaded is reported by the scene renderer, not fatal here.
            let image_directory = self
                .directory
                .path()
                .join("bundle")
                .join(&self.id)
                .join(&page.id);
            let mut images = |name: &str| -> Result<Option<Vec<u8>>> {
                ensure!(notebook::valid_image_name(name), "Invalid image name");
                let path = image_directory.join(name);
                if !path.is_file() {
                    return Ok(None);
                }
                // One byte over the limit still reads, so the scene renderer
                // reports the file as too large rather than unreadable.
                let limit = notebook::MAX_IMAGE_BYTES + 1;
                job::read_bounded(&path, limit).map(Some)
            };
            let rendered =
                notebook::render_svg_with_images(bytes, base_width, base_height, &mut images)?;
            warnings.extend(rendered.warnings.clone());
            Some(rendered)
        } else {
            None
        };
        let mut bounds = ink
            .as_ref()
            .map(|ink| ink.bounds)
            .unwrap_or(notebook::CanvasBounds {
                x: 0.0,
                y: 0.0,
                width: base_width,
                height: base_height,
            });
        if let Some(rect) = placement {
            bounds = union_bounds(bounds, rect);
        }
        let (x, y, width, height) = (bounds.x, bounds.y, bounds.width, bounds.height);
        // A PDF-backed page draws the source page, not a paper template. The
        // annotations-only layer draws neither and is complete without them.
        let mut template_supplied = background.is_some() || !paper;
        let template = if background.is_none() && paper {
            match crate::templates::render(
                &self.template_settings,
                &page.id,
                index,
                base_width,
                base_height,
            ) {
                Ok(template) => {
                    // A Blank or absent template is nothing to supply and
                    // nothing to complain about.
                    template_supplied = true;
                    template
                }
                Err(error) => {
                    warnings.push(format!("Paper template: {error:#}"));
                    None
                }
            }
        } else {
            None
        };
        // The scene never carries its paper. Say so only when this renderer did
        // not supply one either; otherwise the page shown is complete.
        if template_supplied {
            let note = format!(
                "{}{}",
                notebook::NOTE_PREFIX,
                notebook::TEMPLATE_OUTSIDE_SCENE
            );
            warnings.retain(|warning| *warning != note);
        }
        // The raster path fills its pixmap white and composites the decoded
        // background under the tree, so the opaque sheet rectangle would hide
        // it. The exported SVG still carries the rectangle, unchanged.
        let sheet = if embed_background {
            format!(r#"<rect x="{x}" y="{y}" width="{width}" height="{height}" fill="white"/>"#)
        } else {
            String::new()
        };
        let mut svg = format!(
            r#"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" width="{width}" height="{height}" viewBox="{x} {y} {width} {height}">{sheet}"#
        );
        if let Some(template) = template {
            svg.push_str(&template);
        }
        let mut raster_background = None;
        if let (Some(pixmap), Some(rect)) = (background, placement) {
            if embed_background {
                // Place the source page in nominal paper coordinates. Scene
                // extent and visible ink bounds must not resize the PDF
                // background.
                svg.push_str(&format!(r#"<image x="{}" y="{}" width="{}" height="{}" xlink:href="data:image/png;base64,{}"/>"#,
                    rect.x, rect.y, rect.width, rect.height, STANDARD.encode(png_from_pixmap(&pixmap)?)));
            } else {
                // Decoded once; every tile then composites these pixels.
                // The SVG's viewBox starts at (x, y), so resvg renders with
                // that origin at pixel 0. Store the rectangle in the same
                // canvas-relative space the tile transform works in.
                raster_background = Some((
                    pixmap,
                    notebook::CanvasBounds {
                        x: rect.x - x,
                        y: rect.y - y,
                        width: rect.width,
                        height: rect.height,
                    },
                ));
            }
        }
        if let Some(ink) = ink {
            // Position the nested viewport at its actual origin. Without x/y,
            // a negative viewBox is translated twice and cuts off left/top ink.
            let start = ink
                .svg
                .find("<svg")
                .ok_or_else(|| anyhow!("Invalid rendered page"))?;
            let nested = ink.svg[start..].replacen(
                "<svg",
                &format!("<svg x=\"{}\" y=\"{}\"", ink.bounds.x, ink.bounds.y),
                1,
            );
            if embed_background {
                svg.push_str(&nested);
            } else {
                svg.push_str(&raster_friendly_images(&nested));
            }
        }
        svg.push_str("</svg>");
        // Collapse repeats before sorting so a page with forty copies of one
        // note shows it once with its count, not forty times.
        let warnings = notebook::collapse_notes(warnings);
        Ok(Composed {
            svg,
            warnings,
            source_page: raster_background.as_ref().and(page.background),
            background: raster_background,
        })
    }

    /// The source page's box in PDF points, parsed once for the whole document.
    fn page_box(&self, number: usize) -> Result<pdf_geometry::PdfPage> {
        let mut cached = self
            .page_boxes
            .lock()
            .map_err(|_| anyhow!("PDF page sizes are unavailable"))?;
        if cached.is_none() {
            let path = self
                .background_pdf
                .as_ref()
                .ok_or_else(|| anyhow!("This document has no PDF background"))?;
            *cached = Some(page_boxes(path)?);
        }
        cached
            .as_ref()
            .and_then(|boxes| boxes.get(number))
            .copied()
            .ok_or_else(|| anyhow!("Source PDF page {} is missing", number + 1))
    }

    fn background_path(&self, number: usize) -> PathBuf {
        self.directory
            .path()
            .join(format!("background-{number}.{}", self.raster_extension()))
    }

    /// What pdftoppm writes. The viewer and covers take raw PPM: their rasters
    /// are bounded at 1800 pixels and are only decoded again, and encoding PNG
    /// costs several times the rasterizing itself. Stills keep PNG, because a
    /// long page as wide as a still would exceed the sandbox's 32 MiB file
    /// limit uncompressed.
    fn raster_extension(&self) -> &'static str {
        if self.still_side.is_some() {
            "png"
        } else {
            "ppm"
        }
    }

    /// Raster the source PDF page `number`, reusing an earlier raster.
    ///
    /// When `also` names a further page that has no raster yet, both are
    /// produced by a single `pdftoppm -f N -l M`, which parses the PDF once.
    /// The sandbox is unchanged: the same prlimit bounds, the same cleared
    /// environment, the same timeout and the same 32 MiB read bound per page.
    fn rasterize_pdf(&self, pdf: &Path, number: usize, also: Option<usize>) -> Result<()> {
        if self.background_path(number).is_file() {
            return Ok(());
        }
        // Only ever widen the range to an adjacent page that is still missing,
        // so one invocation can never be asked for an unbounded span.
        let last = match also {
            Some(next) if next == number + 1 && !self.background_path(next).is_file() => next,
            _ => number,
        };
        // A long page is rasterized for a still as wide as the still, since
        // fitting its longest side would leave the text a few pixels wide.
        let fit_width = self.still_side.and_then(|side| {
            let page = self.page_box(number).ok()?;
            is_long_page(page.width, page.height)
                .then(|| still_size(page.width, page.height, side).0)
        });
        let last = if fit_width.is_some() { number } else { last };
        let mut args = vec![
            "-f".to_owned(),
            (number + 1).to_string(),
            "-l".to_owned(),
            (last + 1).to_string(),
        ];
        match fit_width {
            Some(width) => args.extend([
                "-scale-to-x".to_owned(),
                width.to_string(),
                "-scale-to-y".to_owned(),
                "-1".to_owned(),
            ]),
            None => args.extend(["-scale-to".to_owned(), self.pdf_longest_side.to_string()]),
        }
        let extension = self.raster_extension();
        if extension == "png" {
            args.push("-png".to_owned());
        }
        // pdftoppm writes `<prefix>-<n>.<ext>` for a range and `<prefix>.<ext>`
        // with -singlefile; use a per-run prefix so concurrent renders of
        // different pages never collide on an output name.
        let prefix = self
            .directory
            .path()
            .join(format!("raster-{number}-{last}"));
        let single = number == last;
        if single {
            args.push("-singlefile".to_owned());
        }
        // Two pages parse the document once but do twice the rasterizing,
        // so allow the same per-page budget rather than loosening it.
        let budget = Duration::from_secs(45 * (last - number + 1) as u64);
        if let Err(error) = run_pdftoppm(&args, pdf, &prefix, budget) {
            for candidate in pdftoppm_outputs(&prefix, number, last, extension) {
                let _ = std::fs::remove_file(candidate);
            }
            return Err(error);
        }
        // Move each produced page to its stable per-page name. A page that
        // pdftoppm did not write simply stays missing and is retried later.
        for (page, produced) in pdftoppm_named(&prefix, number, last, single, extension) {
            if produced.is_file() {
                let _ = std::fs::rename(&produced, self.background_path(page));
            }
        }
        ensure!(
            self.background_path(number).is_file(),
            "Could not render this PDF page"
        );
        Ok(())
    }

    fn pdf_page_with(
        &self,
        pdf: &Path,
        number: usize,
        also: Option<usize>,
    ) -> Result<resvg::tiny_skia::Pixmap> {
        self.rasterize_pdf(pdf, number, also)?;
        let bytes = job::read_bounded(&self.background_path(number), 32 * 1024 * 1024)?;
        decode_raster(&bytes).context("Could not decode the PDF background")
    }

    /// One band of the source PDF page `number`, rasterized as part of the
    /// whole page at `key.width` x `key.height` pixels, from the band cache
    /// when it is there. Tile workers asking for the same band at once wait
    /// for a single Poppler run.
    fn band(
        &self,
        pdf: &Path,
        number: usize,
        key: BandKey,
    ) -> Result<Arc<resvg::tiny_skia::Pixmap>> {
        let cell = {
            let mut bands = self
                .bands
                .lock()
                .map_err(|_| anyhow!("The band cache is unavailable"))?;
            let cell = match bands.iter().position(|(cached, _)| *cached == key) {
                Some(position) => bands.remove(position).map(|(_, cell)| cell),
                None => None,
            }
            .unwrap_or_default();
            bands.push_back((key.clone(), cell.clone()));
            while bands.len() > BAND_CACHE_LIMIT {
                bands.pop_front();
            }
            cell
        };
        cell.get_or_init(|| {
            self.rasterize_band(pdf, number, &key)
                .map(Arc::new)
                .map_err(|error| format!("{error:#}"))
        })
        .clone()
        .map_err(|error| anyhow!(error))
    }

    fn rasterize_band(
        &self,
        pdf: &Path,
        number: usize,
        key: &BandKey,
    ) -> Result<resvg::tiny_skia::Pixmap> {
        let prefix = self.directory.path().join(format!(
            "band-{number}-{}x{}-{}-{}",
            key.width, key.height, key.column, key.row
        ));
        let args = [
            "-f",
            &(number + 1).to_string(),
            "-l",
            &(number + 1).to_string(),
            "-singlefile",
            "-scale-to-x",
            &key.width.to_string(),
            "-scale-to-y",
            &key.height.to_string(),
            "-x",
            &(key.column * BAND_WIDTH).to_string(),
            "-y",
            &(key.row * BAND_HEIGHT).to_string(),
            "-W",
            &BAND_WIDTH.to_string(),
            "-H",
            &BAND_HEIGHT.to_string(),
        ]
        .map(str::to_owned);
        let output = prefix.with_extension("ppm");
        let result = run_pdftoppm(&args, pdf, &prefix, Duration::from_secs(45))
            .and_then(|()| job::read_bounded(&output, 32 * 1024 * 1024))
            .and_then(|bytes| decode_raster(&bytes));
        let _ = std::fs::remove_file(&output);
        result
    }

    /// The rendered page, from the cache when it is there.
    ///
    /// Returns an `Arc` so a tile worker can keep rendering from a page while
    /// the foreground thread renders or prefetches another one.
    pub fn page(&self, index: usize) -> Result<Arc<CachedPage>> {
        if let Some(page) = self.cached_page(index) {
            return Ok(page);
        }
        let page = Arc::new(self.render_page(index)?);
        self.store_page(page.clone());
        Ok(page)
    }

    /// The rendered page only if it is already cached; never renders.
    pub fn cached_page(&self, index: usize) -> Option<Arc<CachedPage>> {
        let mut cache = self.rendered.lock().ok()?;
        let position = cache.iter().position(|page| page.index == index)?;
        // Touch it so the least recently used page is the one evicted.
        let page = cache.remove(position)?;
        cache.push_back(page.clone());
        Some(page)
    }

    fn store_page(&self, page: Arc<CachedPage>) {
        let Ok(mut cache) = self.rendered.lock() else {
            return;
        };
        cache.retain(|cached| cached.index != page.index);
        cache.push_back(page);
        while cache.len() > PAGE_CACHE_LIMIT {
            cache.pop_front();
        }
    }

    /// Build one page's tree and background without touching the cache.
    ///
    /// Used directly by prefetch, which renders a page off the critical path
    /// and hands the result to [`Document::adopt_page`].
    pub fn render_page(&self, index: usize) -> Result<CachedPage> {
        self.render_layers(index, Layers::Everything)
    }

    /// The page's annotations alone: ink, highlights, typed text and inserted
    /// images, without the PDF background or the paper template, on the same
    /// canvas as [`Document::render_page`]. Never cached; the viewer does not
    /// use it.
    pub fn render_annotations(&self, index: usize) -> Result<CachedPage> {
        self.render_layers(index, Layers::Annotations)
    }

    fn render_layers(&self, index: usize, layers: Layers) -> Result<CachedPage> {
        let Composed {
            mut svg,
            mut warnings,
            background,
            source_page,
        } = self.compose_layers(index, false, layers)?;
        let mut options = resvg::usvg::Options::default();
        // Most pages are paths and raster backgrounds only. Scanning every
        // installed font is worth it only for a page with typed text.
        if svg.contains("<text") {
            options.fontdb_mut().load_system_fonts();
            if options.fontdb_mut().is_empty() {
                warnings
                    .push("Typed text was dropped because no system fonts are installed".into());
                svg = strip_text_elements(&svg);
            }
        }
        let tree =
            resvg::usvg::Tree::from_str(&svg, &options).context("Could not rasterize the page")?;
        Ok(CachedPage {
            index,
            tree,
            background,
            source_page,
            warnings,
            overview: Mutex::new(Vec::new()),
        })
    }

    /// Put an already-rendered page into the cache, e.g. one built by prefetch.
    pub fn adopt_page(&self, page: CachedPage) -> Arc<CachedPage> {
        let page = Arc::new(page);
        self.store_page(page.clone());
        page
    }

    /// Render `index` into the cache if it is not there already.
    ///
    /// Used by the background prefetch step: it is exactly [`Document::page`]
    /// with the result dropped, so a later navigation to that page is a cache
    /// hit rather than a render.
    pub fn prefetch_page(&self, index: usize) -> Result<()> {
        if self.is_page_cached(index) {
            return Ok(());
        }
        self.page(index).map(|_| ())
    }

    /// True when this page can be painted without any rendering work.
    pub fn is_page_cached(&self, index: usize) -> bool {
        self.rendered
            .lock()
            .map(|cache| cache.iter().any(|page| page.index == index))
            .unwrap_or(false)
    }

    /// The low-resolution first paint. Tiles supply the detail above this.
    pub const FAST_OVERVIEW: u32 = 600;

    pub fn preview(&self, index: usize) -> Result<Preview> {
        self.overview(index, Self::FAST_OVERVIEW)
    }

    pub fn overview(&self, index: usize, longest_side: u32) -> Result<Preview> {
        let page = self.page(index)?;
        self.raster_overview(&page, longest_side, false)
    }

    /// The largest [`Document::still`].
    pub const MAX_STILL: u32 = 4000;

    /// The whole page on white at `longest_side` pixels on its longer side,
    /// or, for a long page, as wide as a 3:4 page of that size and as tall as
    /// the page needs (see [`still_size`]).
    ///
    /// Unlike [`Document::overview`] it is never cached and may be as large
    /// as [`Document::MAX_STILL`]; it is meant for saving a page as an image.
    pub fn still(&self, page: &CachedPage, longest_side: u32) -> Result<Preview> {
        ensure!(
            (1..=Self::MAX_STILL).contains(&longest_side),
            "Image size must be 1 to {} pixels",
            Self::MAX_STILL
        );
        let size = page.tree.size();
        let (width, height) = still_size(size.width(), size.height(), longest_side);
        let scale = width as f32 / size.width();
        let mut pixmap = resvg::tiny_skia::Pixmap::new(width, height)
            .ok_or_else(|| anyhow!("Page allocation failed"))?;
        page.paint(
            resvg::tiny_skia::Transform::from_scale(scale, scale),
            &mut pixmap,
        );
        Ok(Preview {
            rgba: pixmap.take(),
            width: width as usize,
            height: height as usize,
            page: page.index,
            total: self.pages.len(),
            title: self.title.clone(),
            warnings: page.warnings.clone(),
            document_key: self.document_key.clone(),
            canvas_width: size.width(),
            canvas_height: size.height(),
        })
    }

    /// A long-page cover shows its top section at readable thumbnail density.
    pub fn cover(&self, index: usize) -> Result<Preview> {
        let page = self.page(index)?;
        self.raster_overview(&page, 280, true)
    }

    /// Rasterize a whole page small, for the first paint or a library cover.
    pub fn raster_overview(
        &self,
        page: &CachedPage,
        longest_side: u32,
        cover: bool,
    ) -> Result<Preview> {
        ensure!((1..=1800).contains(&longest_side), "Invalid overview size");
        let size = page.tree.size();
        // Returning to a page repaints its first-paint raster from memory.
        // Covers render once into the on-disk cache, so they are not held.
        let reusable = !cover;
        if reusable {
            if let Ok(cached) = page.overview.lock() {
                if let Some(raster) = cached
                    .iter()
                    .find(|raster| raster.longest_side == longest_side)
                {
                    return Ok(Preview {
                        rgba: raster.rgba.clone(),
                        width: raster.width,
                        height: raster.height,
                        page: page.index,
                        total: self.pages.len(),
                        title: self.title.clone(),
                        warnings: page.warnings.clone(),
                        document_key: self.document_key.clone(),
                        canvas_width: size.width(),
                        canvas_height: size.height(),
                    });
                }
            }
        }
        let region_height = if cover && size.height() / size.width() > 2.0 {
            size.width() * 1.5
        } else {
            size.height()
        };
        let scale = longest_side as f32 / size.width().max(region_height);
        let width = (size.width() * scale).round().max(1.0) as u32;
        let height = (region_height * scale).round().max(1.0) as u32;
        let mut pixmap = resvg::tiny_skia::Pixmap::new(width, height)
            .ok_or_else(|| anyhow!("Page allocation failed"))?;
        page.paint(
            resvg::tiny_skia::Transform::from_scale(scale, scale),
            &mut pixmap,
        );
        let rgba = pixmap.data().to_vec();
        if reusable {
            if let Ok(mut cached) = page.overview.lock() {
                cached.retain(|raster| raster.longest_side != longest_side);
                cached.push(PageRaster {
                    longest_side,
                    rgba: rgba.clone(),
                    width: width as usize,
                    height: height as usize,
                });
                // Two sizes are all the viewer asks for: the cheap first paint
                // and, when a fit needs it, the detail render.
                while cached.len() > OVERVIEW_CACHE_LIMIT {
                    cached.remove(0);
                }
            }
        }
        Ok(Preview {
            rgba,
            width: width as usize,
            height: height as usize,
            page: page.index,
            total: self.pages.len(),
            title: self.title.clone(),
            warnings: page.warnings.clone(),
            document_key: self.document_key.clone(),
            canvas_width: size.width(),
            canvas_height: size.height(),
        })
    }

    /// Rasterize only a bounded viewport tile, never an entire infinite canvas.
    pub fn tile(&self, key: TileKey) -> Result<Tile> {
        let page = self.page(key.page)?;
        self.tile_from(&page, key)
    }

    /// The tile rasterizer itself. Takes an already-rendered page so a tile
    /// worker can serve many tiles from one shared `Arc<CachedPage>`.
    pub fn tile_from(&self, page: &CachedPage, key: TileKey) -> Result<Tile> {
        ensure!(
            key.document_key == self.document_key,
            "Tile belongs to a previous document"
        );
        ensure!(
            (10..=8000).contains(&key.scale_milli),
            "Tile zoom is out of range"
        );
        ensure!(key.page == page.index, "Tile belongs to another page");
        let TileGeometry {
            scale,
            origin: (x, y),
            size: (width, height),
        } = tile_geometry(page, &key)?;
        let mut pixmap = resvg::tiny_skia::Pixmap::new(width, height)
            .ok_or_else(|| anyhow!("Tile allocation failed"))?;
        let transform =
            resvg::tiny_skia::Transform::from_row(scale, 0.0, 0.0, scale, -(x as f32), -(y as f32));
        // A band that cannot be rendered leaves the tile as sharp as the
        // stored background allows, never blank.
        if !self
            .paint_sharp(page, scale, (x, y), transform, &mut pixmap)
            .unwrap_or(false)
        {
            page.paint(transform, &mut pixmap);
        }
        Ok(Tile {
            key,
            rgba: pixmap.data().to_vec(),
            width: width as usize,
            height: height as usize,
        })
    }

    /// Paint a tile whose PDF background needs more detail than the page's
    /// stored raster holds, from bands of the PDF page rendered at the tile's
    /// own resolution: a long page shown at full width, or any page zoomed in.
    ///
    /// Returns `Ok(false)` when the stored raster is fine enough, and leaves
    /// the painting to [`CachedPage::paint`].
    fn paint_sharp(
        &self,
        page: &CachedPage,
        scale: f32,
        (x, y): (u64, u64),
        transform: resvg::tiny_skia::Transform,
        pixmap: &mut resvg::tiny_skia::Pixmap,
    ) -> Result<bool> {
        let (Some(pdf), Some((left, top, keys))) = (
            &self.background_pdf,
            self.sharp_bands(page, scale, (x, y), (pixmap.width(), pixmap.height())),
        ) else {
            return Ok(false);
        };
        let mut bands = Vec::new();
        for key in keys {
            let (column, row) = (i64::from(key.column), i64::from(key.row));
            bands.push((column, row, self.band(pdf, key.page, key)?));
        }
        pixmap.fill(resvg::tiny_skia::Color::WHITE);
        for (column, row, band) in bands {
            pixmap.draw_pixmap(
                (left + column * i64::from(BAND_WIDTH)) as i32,
                (top + row * i64::from(BAND_HEIGHT)) as i32,
                band.as_ref().as_ref(),
                &resvg::tiny_skia::PixmapPaint::default(),
                resvg::tiny_skia::Transform::identity(),
                None,
            );
        }
        resvg::render(&page.tree, transform, &mut pixmap.as_mut());
        Ok(true)
    }

    /// The bands a `width` x `height` tile at `(x, y)` paints its background
    /// from, with the background's top-left pixel in tile pixels, or `None`
    /// when the page's stored raster is fine enough at this scale.
    fn sharp_bands(
        &self,
        page: &CachedPage,
        scale: f32,
        (x, y): (u64, u64),
        (tile_width, tile_height): (u32, u32),
    ) -> Option<(i64, i64, Vec<BandKey>)> {
        let (Some(number), Some((stored, rect))) = (page.source_page, &page.background) else {
            return None;
        };
        let width = (rect.width * scale).round();
        let height = (rect.height * scale).round();
        if width <= stored.width() as f32 * SHARPEN_RATIO
            || !(1.0..=1_000_000.0).contains(&width)
            || !(1.0..=1_000_000.0).contains(&height)
        {
            return None;
        }
        let (width, height) = (width as u32, height as u32);
        // The background's top-left pixel, in tile pixels. Whole pixels keep
        // the bands on the page's own pixel grid, as one raster would be.
        let left = (rect.x * scale).round() as i64 - x as i64;
        let top = (rect.y * scale).round() as i64 - y as i64;
        // The part of the background this tile shows, in background pixels.
        let first_x = (-left).max(0);
        let first_y = (-top).max(0);
        let end_x = (i64::from(tile_width) - left).min(i64::from(width));
        let end_y = (i64::from(tile_height) - top).min(i64::from(height));
        let mut bands = Vec::new();
        if end_x > first_x && end_y > first_y {
            for row in first_y / i64::from(BAND_HEIGHT)..=(end_y - 1) / i64::from(BAND_HEIGHT) {
                for column in first_x / i64::from(BAND_WIDTH)..=(end_x - 1) / i64::from(BAND_WIDTH)
                {
                    bands.push(BandKey {
                        page: number,
                        width,
                        height,
                        column: column as u32,
                        row: row as u32,
                    });
                }
            }
        }
        Some((left, top, bands))
    }

    /// Start rendering, all at once, the bands these tiles will need.
    ///
    /// Tile workers take tiles centre-out, so the first few usually wait on
    /// the same band while the others are not started: a screen that needs
    /// two bands took the time of both. Each band starts here on its own
    /// thread instead, and a tile worker that reaches it waits for that run
    /// rather than starting another. Bands already rendered are skipped, and
    /// never more are started than the band cache keeps.
    pub fn warm_bands(self: &Arc<Self>, keys: &[TileKey]) {
        let Some(pdf) = self.background_pdf.clone() else {
            return;
        };
        let mut wanted: Vec<BandKey> = Vec::new();
        for key in keys {
            if self.accepts_tile(key).is_err() {
                continue;
            }
            let Some(page) = self.cached_page(key.page) else {
                continue;
            };
            let Ok(tile) = tile_geometry(&page, key) else {
                continue;
            };
            for band in self
                .sharp_bands(&page, tile.scale, tile.origin, tile.size)
                .map_or_else(Vec::new, |(_, _, bands)| bands)
            {
                if !wanted.contains(&band) && !self.has_band(&band) {
                    wanted.push(band);
                }
            }
        }
        for band in wanted.into_iter().take(BAND_CACHE_LIMIT) {
            let document = self.clone();
            let pdf = pdf.clone();
            std::thread::spawn(move || {
                // A failure is the tile's to report when it asks again.
                let _ = document.band(&pdf, band.page, band);
            });
        }
    }

    /// Whether this band is in the band cache, rendered or being rendered.
    fn has_band(&self, key: &BandKey) -> bool {
        self.bands
            .lock()
            .is_ok_and(|bands| bands.iter().any(|(cached, _)| cached == key))
    }

    /// Validate a tile key against this document without rendering anything.
    /// The tile worker uses it to reject stale keys before taking a page.
    pub fn accepts_tile(&self, key: &TileKey) -> Result<()> {
        ensure!(
            key.document_key == self.document_key,
            "Tile belongs to a previous document"
        );
        ensure!(
            (10..=8000).contains(&key.scale_milli),
            "Tile zoom is out of range"
        );
        Ok(())
    }

    pub fn export_svg(&self, index: usize, out: &Path) -> Result<()> {
        ensure!(
            out.extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("svg")),
            "Use an .svg filename"
        );
        persist_new(out, self.svg(index)?.0.as_bytes())
    }

    /// Export the same appearance as the viewer; native data remains untouched.
    pub fn export_pdf(&self, out: &Path) -> Result<()> {
        use lopdf::{dictionary, Object, Stream};
        ensure!(
            out.extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("pdf")),
            "Use a .pdf filename"
        );
        ensure!(
            !out.exists(),
            "Choose a new filename; export does not overwrite files"
        );
        let mut pdf = lopdf::Document::with_version("1.5");
        let pages_id = pdf.new_object_id();
        let mut kids = Vec::new();
        let mut image_bytes = 0usize;
        // Bound peak/output size for the deliberately rasterized preview export.
        ensure!(
            self.pages.len() <= 200,
            "Preview PDF export is limited to 200 pages; use the native bundle for larger notebooks"
        );
        let mut tile_count = 0usize;
        for i in 0..self.pages.len() {
            let rendered = self
                .page(i)
                .with_context(|| format!("Could not render page {} for PDF export", i + 1))?;
            let (canvas_width, canvas_height) =
                (rendered.tree.size().width(), rendered.tree.size().height());
            let columns = (canvas_width / TILE_SIZE as f32).ceil() as u32;
            let rows = (canvas_height / TILE_SIZE as f32).ceil() as u32;
            tile_count = tile_count
                .checked_add(columns as usize * rows as usize)
                .ok_or_else(|| anyhow!("PDF export size overflow"))?;
            ensure!(
                tile_count <= 4096,
                "This canvas exceeds the preview PDF rendering budget; export its SVG or native bundle instead"
            );
            // Retain the complete aspect ratio within PDF's portable page-size
            // range. Raster tiles keep their detail instead of squeezing a long
            // page into a single 1800-pixel image.
            let unit = (1.0f32 / 3.0).min(14000.0 / canvas_width.max(canvas_height));
            let width = canvas_width * unit;
            let height = canvas_height * unit;
            let mut xobjects = lopdf::Dictionary::new();
            let mut drawing = String::new();
            for row in 0..rows {
                for column in 0..columns {
                    let tile = self
                        .tile_from(
                            &rendered,
                            TileKey {
                                document_key: self.document_key.clone(),
                                page: i,
                                scale_milli: 1000,
                                column,
                                row,
                            },
                        )
                        .with_context(|| {
                            format!("Could not render page {} for PDF export", i + 1)
                        })?;
                    let rgb: Vec<u8> = tile
                        .rgba
                        .as_chunks::<4>()
                        .0
                        .iter()
                        .flat_map(|p| [p[0], p[1], p[2]])
                        .collect();
                    let mut stream = Stream::new(
                        dictionary! { "Type" => "XObject", "Subtype" => "Image",
                        "Width" => tile.width as i64, "Height" => tile.height as i64,
                        "ColorSpace" => "DeviceRGB", "BitsPerComponent" => 8 },
                        rgb,
                    );
                    stream.compress()?;
                    image_bytes += stream.content.len();
                    ensure!(
                        image_bytes <= 256 * 1024 * 1024,
                        "Preview export exceeds 256 MiB; export SVG or native files instead"
                    );
                    let image_id = pdf.add_object(stream);
                    let name = format!("Tile{column}_{row}");
                    xobjects.set(name.as_bytes(), image_id);
                    let x = column as f32 * TILE_SIZE as f32 * unit;
                    let tile_width = tile.width as f32 * unit;
                    let tile_height = tile.height as f32 * unit;
                    let y = height - row as f32 * TILE_SIZE as f32 * unit - tile_height;
                    drawing.push_str(&format!(
                        "q {tile_width} 0 0 {tile_height} {x} {y} cm /{name} Do Q\n"
                    ));
                }
            }
            let contents = pdf.add_object(Stream::new(dictionary! {}, drawing.into_bytes()));
            let page = pdf.add_object(dictionary! { "Type" => "Page", "Parent" => pages_id,
            "MediaBox" => vec![0.into(), 0.into(), Object::Real(width), Object::Real(height)],
            "Resources" => dictionary! { "XObject" => xobjects }, "Contents" => contents });
            kids.push(Object::Reference(page));
        }
        pdf.objects.insert(
            pages_id,
            dictionary! { "Type" => "Pages", "Kids" => kids.clone(), "Count" => kids.len() as i64 }
                .into(),
        );
        let catalog = pdf.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
        pdf.trailer.set("Root", catalog);
        pdf.compress();
        let mut bytes = Vec::new();
        pdf.save_to(&mut bytes)?;
        persist_new(out, &bytes)
    }
}

/// Which layers of a page to compose.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Layers {
    /// The page as the viewer shows it.
    Everything,
    /// What the user added on the tablet, without background or paper.
    Annotations,
}

/// One composed page: its SVG, its notes, and — for the raster path — the
/// decoded PDF background that the SVG deliberately leaves out.
struct Composed {
    svg: String,
    warnings: Vec<String>,
    background: Option<(resvg::tiny_skia::Pixmap, notebook::CanvasBounds)>,
    /// The source PDF page `background` was rasterized from.
    source_page: Option<usize>,
}

/// Run `pdftoppm <args> <pdf> <prefix>` in the sandbox every raster uses:
/// prlimit address-space, CPU, file-size and descriptor bounds, a cleared
/// environment, and a wall-clock `budget` after which it is killed.
fn run_pdftoppm(args: &[String], pdf: &Path, prefix: &Path, budget: Duration) -> Result<()> {
    let mut child = Command::new("/usr/bin/prlimit")
        .args([
            "--as=1073741824",
            "--cpu=30",
            "--fsize=33554432",
            "--nofile=128",
            "--",
            "/usr/bin/pdftoppm",
        ])
        .args(args)
        .arg(pdf)
        .arg(prefix)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("LANG", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("PDF preview needs poppler-utils and util-linux (pdftoppm and prlimit)")?;
    let started = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            ensure!(
                status.success(),
                "Could not render this PDF page within resource limits. Check that poppler-utils is installed"
            );
            return Ok(());
        }
        if started.elapsed() > budget {
            let _ = child.kill();
            let _ = child.wait();
            bail!("PDF page rendering timed out");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Where a tile sits on its page scaled by `scale`: the page is cut into
/// `TILE_SIZE` squares, smaller along the right and bottom edges.
struct TileGeometry {
    scale: f32,
    /// The tile's top-left pixel on the scaled page.
    origin: (u64, u64),
    size: (u32, u32),
}

fn tile_geometry(page: &CachedPage, key: &TileKey) -> Result<TileGeometry> {
    let scale = key.scale_milli as f32 / 1000.0;
    let width = (page.tree.size().width() * scale).ceil() as u64;
    let height = (page.tree.size().height() * scale).ceil() as u64;
    let x = u64::from(key.column) * TILE_SIZE as u64;
    let y = u64::from(key.row) * TILE_SIZE as u64;
    ensure!(x < width && y < height, "Tile is outside the page");
    Ok(TileGeometry {
        scale,
        origin: (x, y),
        size: (
            (width - x).min(TILE_SIZE as u64) as u32,
            (height - y).min(TILE_SIZE as u64) as u32,
        ),
    })
}

/// Every page's box in the PDF at `path`.
///
/// Poppler answers in a few milliseconds, because it reads only the page
/// tree; parsing the whole file with lopdf took 10 s for a 368 MB scan. The
/// full parse remains for a PDF that declares a `/UserUnit`, which Poppler
/// does not report, and for when `pdfinfo` cannot answer.
fn page_boxes(path: &Path) -> Result<Vec<pdf_geometry::PdfPage>> {
    // The same bound the viewer uses for any downloaded component.
    let bytes = job::read_bounded(path, 512 * 1024 * 1024)?;
    if memchr::memmem::find(&bytes, b"/UserUnit").is_none() {
        if let Ok(boxes) = run_pdfinfo(path).and_then(|out| pdf_geometry::parse_pdfinfo_boxes(&out))
        {
            return Ok(boxes);
        }
    }
    parsed_page_boxes(&bytes)
}

/// Every page's box from a full parse of the PDF.
fn parsed_page_boxes(bytes: &[u8]) -> Result<Vec<pdf_geometry::PdfPage>> {
    let pdf = lopdf::Document::load_mem(bytes)
        .map_err(|_| anyhow!("Could not read this PDF's page sizes"))?;
    let mut boxes = Vec::new();
    for (_, id) in pdf.get_pages() {
        boxes.push(pdf_geometry::page_box(&pdf, id)?);
    }
    ensure!(!boxes.is_empty(), "This PDF has no pages");
    Ok(boxes)
}

/// `pdfinfo -box` for every page, in the same sandbox as [`run_pdftoppm`],
/// with its output bounded at 16 MiB (about 200,000 pages).
fn run_pdfinfo(pdf: &Path) -> Result<String> {
    use std::io::Read;
    const LIMIT: u64 = 16 * 1024 * 1024;
    let mut child = Command::new("/usr/bin/prlimit")
        .args([
            "--as=1073741824",
            "--cpu=30",
            "--fsize=33554432",
            "--nofile=128",
            "--",
            "/usr/bin/pdfinfo",
            "-f",
            "1",
            "-l",
            "1000000",
            "-box",
        ])
        .arg(pdf)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("LANG", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("PDF page sizes need poppler-utils and util-linux (pdfinfo and prlimit)")?;
    // Read on another thread, so a full pipe can never stall the child
    // while this one waits for it to exit.
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("pdfinfo output is unavailable"))?;
    let reader = std::thread::spawn(move || {
        let mut output = Vec::new();
        stdout
            .take(LIMIT + 1)
            .read_to_end(&mut output)
            .map(|_| output)
    });
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if started.elapsed() > Duration::from_secs(45) {
            let _ = child.kill();
            let _ = child.wait();
            bail!("Reading the PDF's page sizes timed out");
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    let output = reader
        .join()
        .map_err(|_| anyhow!("pdfinfo output could not be read"))??;
    ensure!(status.success(), "pdfinfo could not read this PDF");
    ensure!(
        output.len() as u64 <= LIMIT,
        "pdfinfo output exceeds its limit"
    );
    Ok(String::from_utf8_lossy(&output).into_owned())
}

/// Decode a pdftoppm raster, PPM or PNG, into an opaque pixmap.
fn decode_raster(bytes: &[u8]) -> Result<resvg::tiny_skia::Pixmap> {
    if bytes.starts_with(b"P6") {
        return decode_ppm(bytes);
    }
    let decoded = image::load_from_memory(bytes)?.to_rgba8();
    let mut pixmap = resvg::tiny_skia::Pixmap::new(decoded.width().max(1), decoded.height().max(1))
        .ok_or_else(|| anyhow!("PDF background allocation failed"))?;
    ensure!(
        pixmap.data().len() == decoded.as_raw().len(),
        "PDF background raster was malformed"
    );
    // tiny-skia stores premultiplied RGBA; the poppler raster is fully
    // opaque, so the straight copy is already premultiplied.
    pixmap.data_mut().copy_from_slice(decoded.as_raw());
    Ok(pixmap)
}

/// Decode the binary PPM pdftoppm writes: `P6`, width, height and a maximum
/// value of 255, separated by whitespace, then one whitespace byte and the
/// RGB samples.
fn decode_ppm(bytes: &[u8]) -> Result<resvg::tiny_skia::Pixmap> {
    let mut position = 2;
    let mut fields = [0u32; 3];
    for field in &mut fields {
        while bytes.get(position).is_some_and(u8::is_ascii_whitespace) {
            position += 1;
        }
        let start = position;
        while bytes.get(position).is_some_and(u8::is_ascii_digit) {
            position += 1;
        }
        ensure!(
            (1..=9).contains(&(position - start)),
            "PDF raster header is malformed"
        );
        *field = std::str::from_utf8(&bytes[start..position])?.parse()?;
    }
    ensure!(
        bytes.get(position).is_some_and(u8::is_ascii_whitespace),
        "PDF raster header is malformed"
    );
    let [width, height, maximum] = fields;
    ensure!(maximum == 255, "PDF raster is not 8-bit");
    let samples = &bytes[position + 1..];
    ensure!(
        samples.len() as u64 == u64::from(width) * u64::from(height) * 3,
        "PDF raster is truncated"
    );
    let mut pixmap = resvg::tiny_skia::Pixmap::new(width, height)
        .ok_or_else(|| anyhow!("PDF background allocation failed"))?;
    for (pixel, rgb) in pixmap
        .data_mut()
        .as_chunks_mut::<4>()
        .0
        .iter_mut()
        .zip(samples.as_chunks::<3>().0)
    {
        *pixel = [rgb[0], rgb[1], rgb[2], 255];
    }
    Ok(pixmap)
}

/// PNG-encode an opaque background for embedding in an exported SVG.
fn png_from_pixmap(pixmap: &resvg::tiny_skia::Pixmap) -> Result<Vec<u8>> {
    use image::ImageEncoder;
    let rgb: Vec<u8> = pixmap
        .data()
        .as_chunks::<4>()
        .0
        .iter()
        .flat_map(|pixel| [pixel[0], pixel[1], pixel[2]])
        .collect();
    let mut png = Vec::new();
    image::codecs::png::PngEncoder::new_with_quality(
        &mut png,
        image::codecs::png::CompressionType::Fast,
        image::codecs::png::FilterType::Adaptive,
    )
    .write_image(
        &rgb,
        pixmap.width(),
        pixmap.height(),
        image::ExtendedColorType::Rgb8,
    )?;
    Ok(png)
}

/// Where pdftoppm may have written output for `prefix` over `first..=last`.
///
/// With `-singlefile` it writes `<prefix>.<ext>`; for a range it writes
/// `<prefix>-<n>.<ext>`, zero-padded to a width poppler chooses from the
/// source document's page count, so every plausible padding is considered.
fn pdftoppm_outputs(prefix: &Path, first: usize, last: usize, extension: &str) -> Vec<PathBuf> {
    let mut out = vec![prefix.with_extension(extension)];
    let name = prefix.file_name().unwrap_or_default().to_string_lossy();
    let parent = prefix.parent().unwrap_or(Path::new("."));
    for page in first..=last {
        for width in 1..=6 {
            out.push(parent.join(format!(
                "{name}-{:0width$}.{extension}",
                page + 1,
                width = width
            )));
        }
    }
    out
}

/// Pair each page in `first..=last` with the file pdftoppm wrote for it.
fn pdftoppm_named(
    prefix: &Path,
    first: usize,
    last: usize,
    single: bool,
    extension: &str,
) -> Vec<(usize, PathBuf)> {
    if single {
        return vec![(first, prefix.with_extension(extension))];
    }
    let name = prefix.file_name().unwrap_or_default().to_string_lossy();
    let parent = prefix.parent().unwrap_or(Path::new("."));
    (first..=last)
        .filter_map(|page| {
            (1..=6)
                .map(|width| {
                    parent.join(format!(
                        "{name}-{:0width$}.{extension}",
                        page + 1,
                        width = width
                    ))
                })
                .find(|candidate| candidate.is_file())
                .map(|path| (page, path))
        })
        .collect()
}

fn union_bounds(a: notebook::CanvasBounds, b: notebook::CanvasBounds) -> notebook::CanvasBounds {
    let x = a.x.min(b.x);
    let y = a.y.min(b.y);
    notebook::CanvasBounds {
        x,
        y,
        width: (a.x + a.width).max(b.x + b.width) - x,
        height: (a.y + a.height).max(b.y + b.height) - y,
    }
}

fn persist_new(out: &Path, bytes: &[u8]) -> Result<()> {
    let parent = out
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
    tmp.write_all(bytes)?;
    tmp.as_file().sync_all()?;
    tmp.persist_noclobber(out)
        .map_err(|_| anyhow!("Cannot export without overwriting an existing file"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use lopdf::Object;
    use serde_json::json;

    fn scene(points: &[(f32, f32)]) -> Vec<u8> {
        scene_stroke(points, 15, 16, 4)
    }

    fn scene_stroke(points: &[(f32, f32)], tool: u32, width: u16, item: u8) -> Vec<u8> {
        let field = |tag: u8, bytes: &[u8]| {
            let mut out = vec![tag];
            if tag & 15 == 12 {
                out.extend((bytes.len() as u32).to_le_bytes());
            }
            out.extend(bytes);
            out
        };
        let mut packed = Vec::new();
        for &(x, y) in points {
            packed.extend(x.to_le_bytes());
            packed.extend(y.to_le_bytes());
            packed.extend(0u16.to_le_bytes());
            packed.extend(width.to_le_bytes());
            packed.extend([0, 255]);
        }
        let mut line = vec![3];
        line.extend(field(0x14, &tool.to_le_bytes()));
        line.extend(field(0x24, &7u32.to_le_bytes()));
        line.extend(field(0x38, &2.0f64.to_le_bytes()));
        line.extend(field(0x5c, &packed));
        let mut envelope = field(0x1f, &[0, 1]);
        envelope.extend(field(0x2f, &[1, item]));
        envelope.extend(field(0x6c, &line));
        let mut scene = b"reMarkable .lines file, version=6          ".to_vec();
        scene.extend((envelope.len() as u32).to_le_bytes());
        scene.extend([0, 1, 2, 5]);
        scene.extend(envelope);
        scene
    }

    fn scene_with_geometry(
        points: &[(f32, f32)],
        paper: (f64, f64),
        rect: (f64, f64, f64, f64),
    ) -> Vec<u8> {
        let field = |tag: u8, bytes: &[u8]| {
            let mut out = vec![tag];
            if tag & 15 == 12 {
                out.extend((bytes.len() as u32).to_le_bytes());
            }
            out.extend(bytes);
            out
        };
        let mut values = rect.0.to_le_bytes().to_vec();
        values.extend(rect.1.to_le_bytes());
        values.extend(rect.2.to_le_bytes());
        values.extend(rect.3.to_le_bytes());
        let mut background = field(0x1f, &[0, 1]);
        background.extend(field(0x2c, &values));
        let mut info = field(0x6c, &background);
        let mut paper_values = paper.0.to_le_bytes().to_vec();
        paper_values.extend(paper.1.to_le_bytes());
        info.extend(field(0x7c, &paper_values));

        let mut bytes = scene(points);
        bytes.extend((info.len() as u32).to_le_bytes());
        bytes.extend([0, 2, 2, 13]);
        bytes.extend(info);
        bytes
    }

    /// A stroke plus one inserted image in the root group: a v3 image table
    /// naming `name`, and an image item covering `rect` = (x, y, w, h) in
    /// centred scene coordinates.
    fn scene_with_image(points: &[(f32, f32)], name: &str, rect: (f32, f32, f32, f32)) -> Vec<u8> {
        let field = |tag: u8, bytes: &[u8]| {
            let mut out = vec![tag];
            if tag & 15 == 12 {
                out.extend((bytes.len() as u32).to_le_bytes());
            }
            out.extend(bytes);
            out
        };
        let block = |header: [u8; 4], payload: &[u8]| {
            let mut out = (payload.len() as u32).to_le_bytes().to_vec();
            out.extend(header);
            out.extend(payload);
            out
        };
        let key = [7u8; 16];
        let mut string = vec![name.len() as u8, 1];
        string.extend(name.as_bytes());
        let mut register = field(0x1f, &[0, 20]);
        register.extend(field(0x2c, &string));
        let mut entry = key.to_vec();
        entry.extend(field(0x1c, &register));
        let mut table = vec![1u8];
        table.extend(field(0x0c, &entry));
        let (x, y, w, h) = rect;
        let mut vertices = vec![16u8];
        for (px, py, u, v) in [
            (x, y, 0.0f32, 0.0f32),
            (x + w, y, 1.0, 0.0),
            (x + w, y + h, 1.0, 1.0),
            (x, y + h, 0.0, 1.0),
        ] {
            for value in [px, py, u, v] {
                vertices.extend(value.to_le_bytes());
            }
        }
        let mut indices = vec![6u8];
        for index in [0u32, 1, 2, 2, 3, 0] {
            indices.extend(index.to_le_bytes());
        }
        let mut key_register = field(0x1f, &[0, 21]);
        key_register.extend(field(0x2c, &key));
        let mut image = vec![7];
        image.extend(field(0x1c, &key_register));
        image.extend(field(0x2f, &[0, 22]));
        image.extend(field(0x3c, &vertices));
        image.extend(field(0x4c, &indices));
        let mut envelope = field(0x1f, &[0, 1]);
        envelope.extend(field(0x2f, &[1, 9]));
        envelope.extend(field(0x6c, &image));
        let mut bytes = scene(points);
        bytes.extend(block([0, 3, 3, 14], &field(0x1c, &table)));
        bytes.extend(block([0, 2, 2, 15], &envelope));
        bytes
    }

    /// A solid green PNG of the given size.
    fn green_png(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = Vec::new();
        image::RgbImage::from_pixel(width, height, image::Rgb([0, 200, 0]))
            .write_to(
                &mut std::io::Cursor::new(&mut bytes),
                image::ImageFormat::Png,
            )
            .unwrap();
        bytes
    }

    fn green_pixels(rgba: &[u8]) -> usize {
        rgba.as_chunks::<4>()
            .0
            .iter()
            .filter(|p| p[0] < 40 && p[1] > 160 && p[2] < 40)
            .count()
    }

    /// A one-page notebook whose scene and page image files are supplied.
    fn notebook_document_with_files(scene: &[u8], files: &[(&str, &[u8])]) -> Document {
        let directory = tempfile::tempdir().unwrap();
        let id = "00000000-0000-0000-0000-000000000040".to_owned();
        let page = "00000000-0000-0000-0000-000000000041";
        let bundle = directory.path().join("bundle");
        std::fs::create_dir_all(bundle.join(&id).join(page)).unwrap();
        std::fs::write(
            bundle.join(format!("{id}.content")),
            json!({"formatVersion":1,"fileType":"notebook","pages":[page]}).to_string(),
        )
        .unwrap();
        std::fs::write(bundle.join(&id).join(format!("{page}.rm")), scene).unwrap();
        for (name, bytes) in files {
            std::fs::write(bundle.join(&id).join(page).join(name), bytes).unwrap();
        }
        Document::load(directory, id, "Images".into()).unwrap()
    }

    fn notebook_document(points: &[(f32, f32)], landscape: bool) -> Document {
        let directory = tempfile::tempdir().unwrap();
        let id = "00000000-0000-0000-0000-000000000020".to_owned();
        let page = "00000000-0000-0000-0000-000000000021";
        let bundle = directory.path().join("bundle");
        std::fs::create_dir_all(bundle.join(&id)).unwrap();
        std::fs::write(
            bundle.join(format!("{id}.content")),
            json!({
                "formatVersion":1,"fileType":"notebook","pages":[page],
                "orientation": if landscape { "landscape" } else { "portrait" }
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(bundle.join(&id).join(format!("{page}.rm")), scene(points)).unwrap();
        Document::load(directory, id, "Notebook".into()).unwrap()
    }

    fn colored_pixels(tile: &Tile) -> usize {
        tile.rgba
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|p| p[0] > 150 && p[1] < 120 && p[2] < 100 && p[3] == 255)
            .count()
    }

    fn pdf_document() -> Document {
        use lopdf::{dictionary, Object, Stream};
        let directory = tempfile::tempdir().unwrap();
        let bundle = directory.path().join("bundle");
        std::fs::create_dir(&bundle).unwrap();
        let id = "00000000-0000-0000-0000-000000000010".to_string();
        let first = "00000000-0000-0000-0000-000000000011";
        let second = "00000000-0000-0000-0000-000000000012";
        let mut pdf = lopdf::Document::with_version("1.5");
        let pages = pdf.new_object_id();
        let mut kids = Vec::new();
        for color in ["1 0 0", "0 0 1"] {
            let stream = pdf.add_object(Stream::new(
                dictionary! {},
                format!("{color} rg 0 0 100 200 re f").into_bytes(),
            ));
            let page = pdf.add_object(dictionary! {"Type"=>"Page", "Parent"=>pages,
            "MediaBox"=>vec![0.into(),0.into(),100.into(),200.into()], "Contents"=>stream});
            kids.push(Object::Reference(page));
        }
        pdf.objects.insert(
            pages,
            dictionary! {"Type"=>"Pages", "Kids"=>kids, "Count"=>2}.into(),
        );
        let catalog = pdf.add_object(dictionary! {"Type"=>"Catalog", "Pages"=>pages});
        pdf.trailer.set("Root", catalog);
        pdf.save(bundle.join(format!("{id}.pdf"))).unwrap();
        std::fs::write(
            bundle.join(format!("{id}.content")),
            json!({
                "formatVersion":1,"fileType":"pdf","pages":[first,second],"redirectionPageMap":[1,0]
            })
            .to_string(),
        )
        .unwrap();
        Document::load(directory, id, "Test".into()).unwrap()
    }

    /// Poppler's page sizes match a full parse: inherited rotation, a crop
    /// clipped to the media box, and pages in order. A PDF that declares a
    /// `/UserUnit`, which Poppler does not report, takes the full parse.
    #[test]
    fn page_sizes_from_poppler_match_the_full_parse() {
        use lopdf::{dictionary, Object};
        let write = |user_unit: bool| {
            let mut pdf = lopdf::Document::with_version("1.5");
            let pages = pdf.new_object_id();
            let mut first = dictionary! {"Type"=>"Page", "Parent"=>pages,
            "MediaBox"=>vec![0.into(),0.into(),200.into(),100.into()],
            "CropBox"=>vec![10.into(),10.into(),190.into(),90.into()]};
            if user_unit {
                first.set("UserUnit", 2);
            }
            let first = pdf.add_object(first);
            let second = pdf.add_object(dictionary! {"Type"=>"Page", "Parent"=>pages,
            "MediaBox"=>vec![0.into(),0.into(),612.into(),792.into()], "Rotate"=>0});
            pdf.objects.insert(
                pages,
                dictionary! {"Type"=>"Pages", "Rotate"=>90, "Count"=>2,
                "Kids"=>vec![Object::Reference(first), Object::Reference(second)]}
                .into(),
            );
            let catalog = pdf.add_object(dictionary! {"Type"=>"Catalog", "Pages"=>pages});
            pdf.trailer.set("Root", catalog);
            let file = tempfile::NamedTempFile::new().unwrap();
            pdf.save(file.path()).unwrap();
            file
        };
        let sizes = |boxes: Vec<pdf_geometry::PdfPage>| {
            boxes
                .into_iter()
                .map(|page| (page.width, page.height))
                .collect::<Vec<_>>()
        };
        let plain = write(false);
        let expected = vec![(80.0, 180.0), (612.0, 792.0)];
        let from_poppler =
            pdf_geometry::parse_pdfinfo_boxes(&run_pdfinfo(plain.path()).unwrap()).unwrap();
        assert_eq!(sizes(from_poppler), expected, "turned a quarter, cropped");
        let bytes = std::fs::read(plain.path()).unwrap();
        assert_eq!(sizes(parsed_page_boxes(&bytes).unwrap()), expected);
        assert_eq!(sizes(page_boxes(plain.path()).unwrap()), expected);
        // Scaled by /UserUnit, which only the full parse applies.
        let scaled = write(true);
        assert_eq!(
            sizes(page_boxes(scaled.path()).unwrap()),
            vec![(160.0, 360.0), (612.0, 792.0)]
        );
    }

    fn pdf_document_with_scene(scene: &[u8], pdf_size: (i64, i64)) -> Document {
        pdf_document_with_scene_files(scene, pdf_size, &[])
    }

    fn pdf_document_with_scene_files(
        scene: &[u8],
        pdf_size: (i64, i64),
        files: &[(&str, &[u8])],
    ) -> Document {
        let fill = format!("0 0 1 rg 0 0 {} {} re f", pdf_size.0, pdf_size.1);
        pdf_document_with_content(scene, pdf_size, files, &fill)
    }

    /// A one-page PDF drawn by the content stream `content`, under `scene`.
    fn pdf_document_with_content(
        scene: &[u8],
        pdf_size: (i64, i64),
        files: &[(&str, &[u8])],
        content: &str,
    ) -> Document {
        use lopdf::{dictionary, Stream};
        let directory = tempfile::tempdir().unwrap();
        let bundle = directory.path().join("bundle");
        let id = "00000000-0000-0000-0000-000000000030".to_string();
        let page = "00000000-0000-0000-0000-000000000031";
        std::fs::create_dir_all(bundle.join(&id)).unwrap();

        let mut pdf = lopdf::Document::with_version("1.5");
        let pages = pdf.new_object_id();
        let stream = pdf.add_object(Stream::new(dictionary! {}, content.as_bytes().to_vec()));
        let page_id = pdf.add_object(dictionary! {
            "Type" => "Page", "Parent" => pages,
            "MediaBox" => vec![0.into(), 0.into(), pdf_size.0.into(), pdf_size.1.into()],
            "Contents" => stream
        });
        pdf.objects.insert(
            pages,
            dictionary! {"Type"=>"Pages", "Kids"=>vec![Object::Reference(page_id)], "Count"=>1}
                .into(),
        );
        let catalog = pdf.add_object(dictionary! {"Type"=>"Catalog", "Pages"=>pages});
        pdf.trailer.set("Root", catalog);
        pdf.save(bundle.join(format!("{id}.pdf"))).unwrap();
        std::fs::write(
            bundle.join(format!("{id}.content")),
            json!({
                "formatVersion":1,"fileType":"pdf","pages":[page],"originalPageCount":1
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(bundle.join(&id).join(format!("{page}.rm")), scene).unwrap();
        for (name, bytes) in files {
            std::fs::create_dir_all(bundle.join(&id).join(page)).unwrap();
            std::fs::write(bundle.join(&id).join(page).join(name), bytes).unwrap();
        }
        Document::load(directory, id, "Placed PDF".into()).unwrap()
    }

    fn pixel_at(preview: &Preview, x: usize, y: usize) -> &[u8] {
        &preview.rgba[(y * preview.width + x) * 4..][..4]
    }

    #[test]
    fn notebook_ink_renders_inside_the_page_viewer() {
        let doc = notebook_document(&[(0.0, 200.0), (100.0, 250.0)], false);
        // The detail resolution the viewer reaches through tiles.
        let preview = doc.overview(0, 1800).unwrap();
        let ink = preview
            .rgba
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|p| p[0] > 150 && p[1] < 120 && p[2] < 100)
            .count();
        assert!(ink > 100, "Native ink must reach the raster preview");
        assert_eq!(preview.total, 1);
    }

    #[test]
    fn an_inserted_image_reaches_the_svg_the_tiles_and_the_cover() {
        let png = green_png(16, 10);
        let scene = scene_with_image(
            &[(0.0, 100.0), (20.0, 120.0)],
            "a.png",
            (-300.0, 300.0, 400.0, 250.0),
        );
        let doc = notebook_document_with_files(&scene, &[("a.png", &png)]);
        let (svg, warnings) = doc.svg(0).unwrap();
        assert!(svg.contains("xlink:href=\"data:image/png;base64,"), "{svg}");
        assert!(
            !warnings.iter().any(|w| w.contains("inserted image")),
            "{warnings:?}"
        );
        // The image covers x 402..802, y 300..550 on the 1404-wide page.
        let page = doc.page(0).unwrap();
        let tile = doc
            .tile_from(
                &page,
                TileKey {
                    document_key: doc.document_key.clone(),
                    page: 0,
                    scale_milli: 1000,
                    column: 1,
                    row: 0,
                },
            )
            .unwrap();
        // Tile (1, 0) spans x 512..1024, y 0..512: the image's x 512..802,
        // y 300..512 lies inside it.
        let covered = green_pixels(&tile.rgba);
        assert!(
            (290 * 212 * 9 / 10..=290 * 212 * 11 / 10).contains(&covered),
            "{covered}"
        );
        let cover = doc.cover(0).unwrap();
        assert!(green_pixels(&cover.rgba) > 0);
    }

    #[test]
    fn a_missing_inserted_image_file_is_reported_and_the_rest_still_renders() {
        let scene = scene_with_image(
            &[(0.0, 100.0), (20.0, 120.0)],
            "a.png",
            (-300.0, 300.0, 400.0, 250.0),
        );
        let doc = notebook_document_with_files(&scene, &[]);
        let (svg, warnings) = doc.svg(0).unwrap();
        assert!(!svg.contains("<image"));
        assert!(svg.contains("<path"), "the stroke is still drawn");
        assert!(
            warnings.iter().any(|w| w.contains("file is not available")),
            "{warnings:?}"
        );
    }

    #[test]
    fn extended_top_and_bottom_tiles_paint_ink() {
        let doc = notebook_document(
            &[(0.0, -700.0), (20.0, -650.0), (0.0, 2600.0), (20.0, 2650.0)],
            false,
        );
        let overview = doc.overview(0, 100).unwrap();
        assert!(overview.canvas_height > 3300.0);
        let rows = (overview.canvas_height / TILE_SIZE as f32).ceil() as u32;
        let make_key = |row| TileKey {
            document_key: overview.document_key.clone(),
            page: 0,
            scale_milli: 1000,
            column: 1,
            row,
        };
        let top = doc.tile(make_key(0)).unwrap();
        let bottom = doc.tile(make_key(rows - 1)).unwrap();
        assert!(
            colored_pixels(&top) > 0,
            "negative-y ink must paint in the first tile row"
        );
        assert!(
            colored_pixels(&bottom) > 0,
            "extended bottom ink must paint in the last tile row"
        );
    }

    #[test]
    fn extended_left_and_right_tiles_paint_ink() {
        let doc = notebook_document(
            &[
                (-900.0, 200.0),
                (-850.0, 250.0),
                (900.0, 200.0),
                (950.0, 250.0),
            ],
            false,
        );
        let overview = doc.overview(0, 100).unwrap();
        assert!(overview.canvas_width > 1850.0);
        let columns = (overview.canvas_width / TILE_SIZE as f32).ceil() as u32;
        let make_key = |column| TileKey {
            document_key: overview.document_key.clone(),
            page: 0,
            scale_milli: 1000,
            column,
            row: 0,
        };
        let left = doc.tile(make_key(0)).unwrap();
        let right = doc.tile(make_key(columns - 1)).unwrap();
        assert!(
            colored_pixels(&left) > 0,
            "negative-x ink must paint in the first tile column"
        );
        assert!(
            colored_pixels(&right) > 0,
            "extended right ink must paint in the last tile column"
        );
    }

    #[test]
    fn landscape_canvas_and_preview_keep_their_aspect_ratio() {
        let doc = notebook_document(&[], true);
        let preview = doc.overview(0, 1800).unwrap();
        assert_eq!(
            (preview.canvas_width, preview.canvas_height),
            (1872.0, 1404.0)
        );
        assert_eq!((preview.width, preview.height), (1800, 1350));
        // The first paint keeps the same shape at the cheap resolution.
        let fast = doc.preview(0).unwrap();
        assert_eq!((fast.width, fast.height), (600, 450));
    }

    #[test]
    fn a_wide_pdf_keeps_its_physical_size_instead_of_the_nominal_paper_width() {
        // 200x100 pt is a wide page. Its scene rectangle is its physical size,
        // centred on the scene origin with its top edge on y = 0.
        let placed = pdf_geometry::place(
            pdf_geometry::PdfPage::new([0.0, 0.0, 200.0, 100.0], None, 0, 1.0).unwrap(),
            (1404.0, 1872.0),
        )
        .unwrap();
        let expect = |value: f32, want: f32| assert!((value - want).abs() < 0.001, "{value}");
        expect(placed.background.width, 200.0 * 227.0 / 72.0);
        expect(placed.background.height, 100.0 * 227.0 / 72.0);
        expect(placed.background.y, 0.0);
        // Centred on scene x = 0, which is canvas x = 702.
        expect(placed.background.x + placed.background.width / 2.0, 702.0);
        expect(placed.background.width / placed.background.height, 2.0);
    }

    #[test]
    fn a_tall_pdf_is_not_letterboxed_into_the_nominal_paper() {
        let placed = pdf_geometry::place(
            pdf_geometry::PdfPage::new([0.0, 0.0, 100.0, 200.0], None, 0, 1.0).unwrap(),
            (1404.0, 1872.0),
        )
        .unwrap();
        let expect = |value: f32, want: f32| assert!((value - want).abs() < 0.001, "{value}");
        expect(placed.background.width, 100.0 * 227.0 / 72.0);
        expect(placed.background.height, 200.0 * 227.0 / 72.0);
        // No vertical padding is inserted; the top edge is the scene origin.
        expect(placed.background.y, 0.0);
    }

    #[test]
    fn saved_scene_extent_does_not_reposition_pdf_or_ink() {
        let bytes = scene_with_geometry(
            &[(-1002.0, 590.0), (-1002.0, 610.0)],
            (1404.0, 1872.0),
            (-1200.0, -300.0, 2400.0, 2400.0),
        );
        let doc = pdf_document_with_scene(&bytes, (200, 100));
        let (svg, warnings) = doc.svg(0).unwrap();
        assert!(warnings
            .iter()
            .all(|warning| !warning.contains("nominal page")));
        // 200x100 pt -> 630.556 x 315.278 scene units, centred on canvas 702.
        assert!(
            svg.contains(r#"<image x="386.72223" y="0" width="630.55554" height="315.27777""#),
            "{}",
            &svg[..svg.find("xlink:href").unwrap_or(svg.len()).min(700)]
        );
        assert!(svg.contains("<svg x=\"-302\" y=\"0\""), "{svg}");

        let preview = doc.overview(0, 1800).unwrap();
        // The saved scene extent still does not resize anything: the canvas is
        // the union of the ink bounds and the background rectangle.
        assert!((preview.canvas_width - 1706.0).abs() < 0.01);
        assert!((preview.canvas_height - 1872.0).abs() < 0.01);
        // The negative-x ink remains independently visible.
        assert!(pixel_at(&preview, 2, 577)[0] > 150);
        assert!(pixel_at(&preview, 2, 577)[1] < 120);
    }

    #[test]
    fn eraser_reveals_pdf_background_without_painting_white() {
        let mut bytes = scene_stroke(&[(-200.0, 100.0), (200.0, 100.0)], 15, 16, 4);
        let eraser = scene_stroke(&[(0.0, 80.0), (0.0, 120.0)], 6, 160, 5);
        bytes.extend_from_slice(&eraser[43..]);
        let doc = pdf_document_with_scene(&bytes, (100, 200));
        let tile = doc
            .tile(TileKey {
                document_key: doc.document_key.clone(),
                page: 0,
                scale_milli: 1000,
                column: 1,
                row: 0,
            })
            .unwrap();
        let pixel =
            |x: usize, y: usize| &tile.rgba[(y * tile.width + x) * 4..(y * tile.width + x + 1) * 4];
        assert_eq!(pixel(190, 100), &[0, 0, 255, 255]);
        let ink = pixel(140, 100);
        assert!(ink[0] > 150 && ink[1] < 120 && ink[2] < 100);
    }

    #[test]
    fn the_annotation_layer_keeps_the_page_geometry_without_the_background() {
        let doc = pdf_document_with_scene(&scene(&[(-200.0, 100.0), (200.0, 100.0)]), (100, 200));
        let full = doc.still(&doc.render_page(0).unwrap(), 1000).unwrap();
        let ink = doc
            .still(&doc.render_annotations(0).unwrap(), 1000)
            .unwrap();
        assert_eq!((full.width, full.height), (ink.width, ink.height));
        assert_eq!(full.width.max(full.height), 1000);
        let blue = |preview: &Preview| {
            preview
                .rgba
                .as_chunks::<4>()
                .0
                .iter()
                .filter(|p| p[0] < 40 && p[1] < 40 && p[2] > 200)
                .count()
        };
        let red = |preview: &Preview| {
            preview
                .rgba
                .as_chunks::<4>()
                .0
                .iter()
                .filter(|p| p[0] > 150 && p[1] < 120 && p[2] < 100)
                .count()
        };
        assert!(blue(&full) > 1000, "the complete page draws its PDF");
        assert_eq!(blue(&ink), 0, "the annotation layer has no PDF");
        assert!(red(&ink) > 50, "the annotation layer keeps the ink");
        // The stroke sits at the same pixels in both images.
        let ink_pixels: Vec<usize> = (0..ink.width * ink.height)
            .filter(|&i| {
                let p = &ink.rgba[i * 4..i * 4 + 4];
                p[0] > 150 && p[1] < 120 && p[2] < 100
            })
            .collect();
        let same = ink_pixels
            .iter()
            .filter(|&&i| {
                let p = &full.rgba[i * 4..i * 4 + 4];
                p[0] > 150 && p[1] < 120 && p[2] < 100
            })
            .count();
        assert!(
            same * 10 >= ink_pixels.len() * 9,
            "{same} of {}",
            ink_pixels.len()
        );
        assert!(doc.still(&doc.render_page(0).unwrap(), 4001).is_err());
    }

    #[test]
    fn huge_long_page_is_served_as_bounded_edge_tiles() {
        let doc = notebook_document(&[(0.0, 200_000.0), (20.0, 200_020.0)], false);
        let preview = doc.overview(0, 100).unwrap();
        assert!(preview.canvas_height > 200_000.0);
        let last_row = (preview.canvas_height / TILE_SIZE as f32).ceil() as u32 - 1;
        let tile = doc
            .tile(TileKey {
                document_key: preview.document_key,
                page: 0,
                scale_milli: 1000,
                column: 1,
                row: last_row,
            })
            .unwrap();
        assert!(tile.width <= TILE_SIZE && tile.height <= TILE_SIZE);
        assert_eq!(tile.rgba.len(), tile.width * tile.height * 4);
        assert!(colored_pixels(&tile) > 0);
    }

    #[test]
    fn rejects_stale_invalid_and_out_of_page_tile_keys() {
        let doc = notebook_document(&[], false);
        let valid = TileKey {
            document_key: doc.document_key.clone(),
            page: 0,
            scale_milli: 1000,
            column: 0,
            row: 0,
        };
        assert!(doc.tile(valid.clone()).is_ok());
        for invalid in [
            TileKey {
                document_key: "stale".into(),
                ..valid.clone()
            },
            TileKey {
                page: 1,
                ..valid.clone()
            },
            TileKey {
                scale_milli: 9,
                ..valid.clone()
            },
            TileKey {
                scale_milli: 8001,
                ..valid.clone()
            },
            TileKey {
                column: u32::MAX,
                ..valid.clone()
            },
            TileKey {
                row: u32::MAX,
                ..valid.clone()
            },
        ] {
            assert!(doc.tile(invalid).is_err());
        }
    }

    #[test]
    fn long_page_cover_keeps_top_ink_readable_without_shortening_the_viewer() {
        let doc = notebook_document(&[(0.0, 100.0), (20.0, 200_020.0)], false);
        let cover = doc.cover(0).unwrap();
        assert_eq!((cover.width, cover.height), (187, 280));
        // Four-unit ink is subpixel at cover scale: detect its antialiased
        // red tint rather than requiring the opaque full-resolution color.
        assert!(cover
            .rgba
            .as_chunks::<4>()
            .0
            .iter()
            .any(|pixel| pixel[0].saturating_sub(pixel[1]) > 20
                && pixel[0].saturating_sub(pixel[2]) > 20));
        let preview = doc.overview(0, 1800).unwrap();
        assert!(preview.canvas_height > 200_000.0);
        assert_eq!(preview.height, 1800);
        // The fast first paint is the same page at a cheaper size.
        assert_eq!(
            doc.preview(0).unwrap().height,
            Document::FAST_OVERVIEW as usize
        );
    }

    #[test]
    fn pdf_export_preserves_the_complete_canvas_aspect_ratio() {
        let doc = notebook_document(&[(0.0, -300.0), (20.0, 3000.0)], false);
        let preview = doc.overview(0, 100).unwrap();
        let output = tempfile::tempdir().unwrap();
        let path = output.path().join("long.pdf");
        doc.export_pdf(&path).unwrap();
        let pdf = lopdf::Document::load(path).unwrap();
        let (_, page_id) = pdf.get_pages().into_iter().next().unwrap();
        let page = pdf.get_object(page_id).unwrap().as_dict().unwrap();
        let media = page.get(b"MediaBox").unwrap().as_array().unwrap();
        let number = |value: &Object| match value {
            Object::Integer(n) => *n as f32,
            Object::Real(n) => *n,
            other => panic!("unexpected PDF number {other:?}"),
        };
        let width = number(&media[2]);
        let height = number(&media[3]);
        assert!((width / height - preview.canvas_width / preview.canvas_height).abs() < 0.0001);
        let resources = page.get(b"Resources").unwrap().as_dict().unwrap();
        let xobjects = resources.get(b"XObject").unwrap().as_dict().unwrap();
        let expected = (preview.canvas_width / TILE_SIZE as f32).ceil() as usize
            * (preview.canvas_height / TILE_SIZE as f32).ceil() as usize;
        assert_eq!(
            xobjects.len(),
            expected,
            "every canvas tile must be embedded"
        );
    }

    #[test]
    fn an_empty_active_page_map_does_not_resurrect_source_pdf_pages() {
        let doc = pdf_document();
        std::fs::write(
            doc.directory
                .path()
                .join("bundle")
                .join(format!("{}.content", doc.id)),
            json!({"formatVersion":2,"fileType":"pdf","cPages":{"uuids":[],"pages":[]}})
                .to_string(),
        )
        .unwrap();
        assert!(Document::load(doc.directory, doc.id, doc.title).is_err());
    }

    #[test]
    fn pdf_page_redirection_and_export_preserve_viewer_appearance() {
        // This is an integration test of the actual PDF rasterizer used by the app.
        // Run explicitly on machines with the documented Poppler runtime installed.
        let doc = pdf_document();
        let scene_path = doc
            .directory
            .path()
            .join("bundle")
            .join(&doc.id)
            .join(format!("{}.rm", doc.pages[0].id));
        std::fs::create_dir_all(scene_path.parent().unwrap()).unwrap();
        std::fs::write(&scene_path, scene(&[(0.0, -300.0), (20.0, -250.0)])).unwrap();
        let annotated_svg = doc.svg(0).unwrap().0;
        // 100x200 pt at 227/72 is 315.278 x 630.556 scene units, centred on
        // canvas x = 702. The canvas is the union with the negative-y ink.
        assert!(
            annotated_svg
                .contains(r#"<image x="544.3611" y="0" width="315.27777" height="630.55554""#),
            "{}",
            &annotated_svg[..annotated_svg
                .find("xlink:href")
                .unwrap_or(annotated_svg.len())
                .min(700)]
        );
        let first = doc.overview(0, 1800).unwrap();
        // Ink at y = -302 and the 1872-unit paper bound the canvas; the small
        // background sits inside it rather than defining it.
        assert_eq!((first.canvas_width, first.canvas_height), (1404.0, 2174.0));
        // A point inside the background rectangle is the source page colour.
        let background_y = ((302.0 + 300.0) / first.canvas_height * first.height as f32) as usize;
        let pixel = &first.rgba[(background_y * first.width + first.width / 2) * 4..][..4];
        assert_eq!(pixel, [0, 0, 255, 255]);
        let second = doc.overview(1, 1800).unwrap();
        // The redirected page keeps its own source colour. Probe the middle of
        // the background rectangle, which no longer fills the nominal paper.
        let scale = second.width as f32 / second.canvas_width;
        let x = (702.0 * scale) as usize;
        let y = (315.278 * scale) as usize;
        let pixel = &second.rgba[(y * second.width + x) * 4..][..4];
        assert_eq!(pixel, [255, 0, 0, 255]);
        let output = tempfile::tempdir().unwrap();
        let pdf = output.path().join("preview.pdf");
        doc.export_pdf(&pdf).unwrap();
        assert_eq!(lopdf::Document::load(&pdf).unwrap().get_pages().len(), 2);
        assert!(doc.export_pdf(&pdf).is_err());
        let svg = output.path().join("page.svg");
        doc.export_svg(0, &svg).unwrap();
        assert!(std::fs::read_to_string(&svg)
            .unwrap()
            .contains("data:image/png;base64,"));
        assert!(doc.export_svg(0, &svg).is_err());
        assert!(doc.preview(2).is_err());
        assert!(doc.overview(2, 1800).is_err());
    }

    #[test]
    fn the_reference_bundle_places_its_background_where_registration_measured_it() {
        // Synthetic stand-in for a measured reference PDF: the same
        // 1053x1404 pt page and 1404x1872 scene paper, so the placement must
        // match the rectangle measured on the tablet.
        let doc = pdf_document_with_scene(&scene(&[(0.0, 100.0), (20.0, 200.0)]), (1053, 1404));
        let (svg, warnings) = doc.svg(0).unwrap();
        assert!(
            warnings.iter().all(|w| !w.contains("not placed")),
            "{warnings:?}"
        );
        // Canvas x = scene x + 702, so -1659.9375 + 702 = -957.9375.
        assert!(
            svg.contains(r#"<image x="-957.9375" y="0" width="3319.875" height="4426.5""#),
            "unexpected placement in {}",
            &svg[..svg.find("xlink:href").unwrap_or(svg.len()).min(600)]
        );
        let preview = doc.preview(0).unwrap();
        // The canvas is a property of the page, not of the raster size.
        assert!((preview.canvas_width - 3319.875).abs() < 0.01);
        assert!((preview.canvas_height - 4426.5).abs() < 0.01);
    }

    /// Render the real reference document when its native files are available.
    ///
    /// Set `REMARKABLE_SAMPLE_BUNDLE` to a directory holding the document's
    /// `.content`, `.metadata`, `.pdf` and page `.rm` files, and optionally
    /// `REMARKABLE_SAMPLE_OUT` to a PNG path. Skipped otherwise, because the
    /// sample is private data that is not part of this repository.
    #[test]
    fn renders_the_reference_sample_when_one_is_supplied() {
        let Ok(source) = std::env::var("REMARKABLE_SAMPLE_BUNDLE") else {
            return;
        };
        let source = std::path::PathBuf::from(source);
        let id = std::fs::read_dir(&source)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .find_map(|entry| {
                let path = entry.path();
                (path.extension()? == "content")
                    .then(|| path.file_stem().unwrap().to_string_lossy().into_owned())
            })
            .expect("sample directory needs a .content file");
        let directory = tempfile::tempdir().unwrap();
        let bundle = directory.path().join("bundle");
        std::fs::create_dir_all(bundle.join(&id)).unwrap();
        for entry in walk(&source) {
            let relative = entry.strip_prefix(&source).unwrap();
            let target = bundle.join(relative);
            std::fs::create_dir_all(target.parent().unwrap()).unwrap();
            std::fs::copy(&entry, &target).unwrap();
        }
        let doc = Document::load(directory, id, "Reference".into()).unwrap();
        // Alignment is checked at the detail resolution the viewer reaches
        // through tiles, not at the cheap first-paint size.
        let preview = doc.overview(0, 1800).unwrap();
        let out = std::env::var("REMARKABLE_SAMPLE_OUT")
            .unwrap_or_else(|_| "/tmp/remarkable-sample-after.png".into());
        image::RgbaImage::from_raw(
            preview.width as u32,
            preview.height as u32,
            preview.rgba.clone(),
        )
        .unwrap()
        .save(&out)
        .unwrap();
        println!("wrote {out} ({}x{})", preview.width, preview.height);
        println!(
            "canvas {} x {}",
            preview.canvas_width, preview.canvas_height
        );
        for warning in &preview.warnings {
            println!("warning: {warning}");
        }
        assert!(preview.width > 0 && preview.height > 0);
    }

    /// Print one real page's notes and warnings, split by severity, and save
    /// its raster. Skipped unless `REMARKABLE_NOTES_BUNDLE` points at a bundle
    /// directory, so this stays out of ordinary test runs.
    ///
    /// ```sh
    /// REMARKABLE_NOTES_BUNDLE=/tmp/remarkable-quick-sheets/bundle \
    ///   REMARKABLE_NOTES_PAGE=1 REMARKABLE_NOTES_OUT=/tmp/page.png \
    ///   cargo test -p turborm --lib prints_a_real_pages_notes -- --nocapture
    /// ```
    #[test]
    fn prints_a_real_pages_notes_when_a_bundle_is_supplied() {
        let Ok(source) = std::env::var("REMARKABLE_NOTES_BUNDLE") else {
            return;
        };
        let source = PathBuf::from(source);
        let id = std::fs::read_dir(&source)
            .expect("bundle directory")
            .flatten()
            .find_map(|entry| {
                let path = entry.path();
                (path.extension()? == "content")
                    .then(|| path.file_stem()?.to_str().map(str::to_owned))
                    .flatten()
            })
            .expect("bundle needs a .content file");
        let directory = tempfile::tempdir().unwrap();
        let bundle = directory.path().join("bundle");
        std::fs::create_dir_all(bundle.join(&id)).unwrap();
        for entry in walk(&source) {
            let relative = entry.strip_prefix(&source).unwrap();
            let target = bundle.join(relative);
            std::fs::create_dir_all(target.parent().unwrap()).unwrap();
            std::fs::copy(&entry, &target).unwrap();
        }
        let doc = Document::load(directory, id, "Notes sample".into()).unwrap();
        let index = std::env::var("REMARKABLE_NOTES_PAGE")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(0);
        let preview = doc.preview(index).unwrap();
        println!("page {} of {}", index + 1, preview.total);
        let classified = preview
            .warnings
            .iter()
            .map(|warning| notebook::severity(warning))
            .collect::<Vec<_>>();
        let warnings = classified
            .iter()
            .filter(|note| note.severity == notebook::Severity::Warning)
            .collect::<Vec<_>>();
        let notes = classified
            .iter()
            .filter(|note| note.severity == notebook::Severity::Note)
            .collect::<Vec<_>>();
        println!("warnings ({}):", warnings.len());
        for entry in &warnings {
            println!("  ! {}", entry.text);
        }
        println!("notes ({}):", notes.len());
        for entry in &notes {
            println!("  - {}", entry.text);
        }
        if let Ok(out) = std::env::var("REMARKABLE_NOTES_OUT") {
            image::RgbaImage::from_raw(
                preview.width as u32,
                preview.height as u32,
                preview.rgba.clone(),
            )
            .unwrap()
            .save(&out)
            .unwrap();
            println!("wrote {out} ({}x{})", preview.width, preview.height);
        }
    }

    #[test]
    fn the_page_cache_keeps_the_last_four_pages() {
        let doc = notebook_document(&[(0.0, 100.0), (20.0, 200.0)], false);
        assert!(!doc.is_page_cached(0));
        let first = doc.page(0).unwrap();
        assert!(doc.is_page_cached(0));
        // A second request returns the very same rendered page, not a re-render.
        assert!(Arc::ptr_eq(&first, &doc.page(0).unwrap()));
        assert!(Arc::ptr_eq(&first, &doc.cached_page(0).unwrap()));
        assert!(doc.cached_page(1).is_none());

        // Beyond the bound, the least recently used page is the one dropped.
        for index in 0..PAGE_CACHE_LIMIT + 2 {
            doc.adopt_page(CachedPage {
                index,
                tree: first.tree.clone(),
                background: None,
                source_page: None,
                warnings: Vec::new(),
                overview: Mutex::new(Vec::new()),
            });
        }
        let held = (0..PAGE_CACHE_LIMIT + 2)
            .filter(|index| doc.is_page_cached(*index))
            .collect::<Vec<_>>();
        assert_eq!(held, vec![2, 3, 4, 5], "the four most recent pages remain");

        // Touching a cached page makes it the most recent, so the next
        // eviction takes a different one.
        doc.cached_page(2).unwrap();
        doc.adopt_page(CachedPage {
            index: 9,
            tree: first.tree.clone(),
            background: None,
            source_page: None,
            warnings: Vec::new(),
            overview: Mutex::new(Vec::new()),
        });
        assert!(doc.is_page_cached(2), "the revisited page survives");
        assert!(!doc.is_page_cached(3));
    }

    #[test]
    fn a_returning_page_repaints_its_first_paint_without_rasterizing_again() {
        let doc = notebook_document(&[(0.0, 100.0), (20.0, 200.0)], false);
        let first = doc.preview(0).unwrap();
        let again = doc.preview(0).unwrap();
        assert_eq!(first.rgba, again.rgba, "the same raster is reused");
        assert_eq!((first.width, first.height), (again.width, again.height));
        // A different size is rendered rather than served from the cache.
        let detail = doc.overview(0, 1800).unwrap();
        assert_ne!(detail.width, first.width);
    }

    #[test]
    fn prefetching_a_page_makes_the_next_view_a_cache_hit() {
        let doc = pdf_document();
        doc.page(0).unwrap();
        assert!(!doc.is_page_cached(1));
        doc.prefetch_page(1).unwrap();
        assert!(doc.is_page_cached(1));
        // Prefetching an already-cached page renders nothing again.
        let held = doc.cached_page(1).unwrap();
        doc.prefetch_page(1).unwrap();
        assert!(Arc::ptr_eq(&held, &doc.cached_page(1).unwrap()));
        assert!(doc.prefetch_page(9).is_err());
    }

    #[test]
    fn one_pdftoppm_run_rasters_the_shown_page_and_the_next() {
        // A two-page PDF whose notebook pages follow the source order.
        let doc = pdf_document();
        std::fs::write(
            doc.directory
                .path()
                .join("bundle")
                .join(format!("{}.content", doc.id)),
            json!({"formatVersion":1,"fileType":"pdf",
                "pages":["00000000-0000-0000-0000-000000000011",
                         "00000000-0000-0000-0000-000000000012"],
                "redirectionPageMap":[0,1]})
            .to_string(),
        )
        .unwrap();
        let doc = Document::load(doc.directory, doc.id, doc.title).unwrap();
        assert_eq!(doc.pages[0].background, Some(0));
        assert_eq!(doc.pages[1].background, Some(1));

        // Showing page 1 leaves page 2's background ready as well, so the page
        // change pays for no further Poppler invocation.
        doc.page(0).unwrap();
        assert!(doc.background_path(0).is_file(), "the shown page");
        assert!(
            doc.background_path(1).is_file(),
            "the adjacent source page must come from the same run"
        );

        // Both pages keep their own colour: the range run produces two images,
        // not one shared raster.
        let probe = |preview: &Preview| {
            let scale = preview.width as f32 / preview.canvas_width;
            let x = (702.0 * scale) as usize;
            let y = (315.278 * scale) as usize;
            preview.rgba[(y * preview.width + x) * 4..][..3].to_vec()
        };
        assert_eq!(probe(&doc.overview(0, 200).unwrap()), vec![255, 0, 0]);
        assert_eq!(probe(&doc.overview(1, 200).unwrap()), vec![0, 0, 255]);

        // A single-page PDF has no neighbour to amortize against and still
        // renders through the same path.
        let single = pdf_document_with_scene(&scene(&[(0.0, 100.0)]), (100, 200));
        single.page(0).unwrap();
        assert!(single.background_path(0).is_file());
    }

    /// The composited background must match what resvg produces from the
    /// embedded `<image>`, so the faster tile path is a pure optimization.
    ///
    /// Runs on a real bundle when `REMARKABLE_BENCH_BUNDLE` points at one, and
    /// on the synthetic placed-PDF documents otherwise.
    #[test]
    fn composited_background_tiles_match_the_embedded_image_path() {
        let docs: Vec<Document> = match std::env::var("REMARKABLE_BENCH_BUNDLE") {
            Ok(source) => vec![bundle_document(&PathBuf::from(source), "Equivalence")],
            Err(_) => vec![
                pdf_document_with_scene(&scene(&[(0.0, 100.0), (20.0, 200.0)]), (1053, 1404)),
                pdf_document_with_scene(&scene(&[(-200.0, 100.0), (200.0, 300.0)]), (100, 200)),
                // A PDF background under an inserted scene image: the image
                // stays in the ink tree, above the composited background.
                pdf_document_with_scene_files(
                    &scene_with_image(
                        &[(0.0, 100.0), (20.0, 200.0)],
                        "a.png",
                        (-300.0, 300.0, 400.0, 250.0),
                    ),
                    (1053, 1404),
                    &[("a.png", &green_png(16, 10))],
                ),
            ],
        };
        for doc in &docs {
            if doc.pages[0].background.is_none() {
                continue;
            }
            // The reference: the exported SVG, with its base64 background,
            // rasterized by resvg exactly as the previous viewer did.
            let (svg, _) = doc.svg(0).unwrap();
            let reference =
                resvg::usvg::Tree::from_str(&svg, &resvg::usvg::Options::default()).unwrap();
            let fast = doc.page(0).unwrap();
            assert_eq!(
                (reference.size().width(), reference.size().height()),
                (fast.size().width(), fast.size().height()),
                "the two paths must agree on the canvas"
            );
            let columns = (fast.size().width() / TILE_SIZE as f32).ceil() as u32;
            let rows = (fast.size().height() / TILE_SIZE as f32).ceil() as u32;
            let mut worst = 0u8;
            let mut checked = 0usize;
            let mut image_pixels = 0usize;
            for row in 0..rows.min(4) {
                for column in 0..columns.min(4) {
                    let key = TileKey {
                        document_key: doc.document_key.clone(),
                        page: 0,
                        scale_milli: 1000,
                        column,
                        row,
                    };
                    let tile = doc.tile_from(&fast, key).unwrap();
                    image_pixels += green_pixels(&tile.rgba);
                    let x = column as u64 * TILE_SIZE as u64;
                    let y = row as u64 * TILE_SIZE as u64;
                    let mut expected =
                        resvg::tiny_skia::Pixmap::new(tile.width as u32, tile.height as u32)
                            .unwrap();
                    expected.fill(resvg::tiny_skia::Color::WHITE);
                    resvg::render(
                        &reference,
                        resvg::tiny_skia::Transform::from_row(
                            1.0,
                            0.0,
                            0.0,
                            1.0,
                            -(x as f32),
                            -(y as f32),
                        ),
                        &mut expected.as_mut(),
                    );
                    for (a, b) in tile.rgba.iter().zip(expected.data()) {
                        worst = worst.max(a.abs_diff(*b));
                    }
                    checked += 1;
                }
            }
            assert!(checked > 0, "at least one tile must be compared");
            if svg.matches("data:image/png;base64").count() == 2 {
                // The inserted image really is in the composited tiles, not
                // only in the exported SVG next to the PDF background.
                assert!(image_pixels > 50_000, "{image_pixels}");
            }
            // The composite uses the same pattern, filter and rectangle as
            // resvg's `<image>`, so the two rasters agree exactly. A tolerance
            // is kept only so a future filter change is a warning, not a break.
            assert!(
                worst <= 2,
                "composited tiles drifted from the embedded-image render by {worst}"
            );
            println!("{checked} tiles compared, worst channel difference {worst}");
        }
    }

    /// Wall-clock timings for the open/page-change path.
    ///
    /// Set `REMARKABLE_BENCH_BUNDLE` to a bundle directory (the same layout the
    /// sample test uses). Skipped otherwise, because the bundles are private
    /// data that is not part of this repository.
    ///
    /// ```sh
    /// REMARKABLE_BENCH_BUNDLE=/tmp/remarkable-castle-alignment/bundle \
    ///   cargo test -p turborm --lib benchmarks_the_viewer_render_path -- --nocapture
    /// ```
    #[test]
    fn benchmarks_the_viewer_render_path() {
        let Ok(source) = std::env::var("REMARKABLE_BENCH_BUNDLE") else {
            return;
        };
        let bundle = PathBuf::from(source);
        let label = std::env::var("REMARKABLE_BENCH_LABEL").unwrap_or_else(|_| "bundle".into());
        let mut rows: Vec<(String, Duration)> = Vec::new();
        let time = |what: &mut dyn FnMut()| {
            let started = Instant::now();
            what();
            started.elapsed()
        };

        // --- Stages, each on a document that has done nothing before it. ---
        let doc = bundle_document(&bundle, "Benchmark");
        let page0 = doc.pages[0].clone();
        if let (Some(pdf), Some(number)) = (doc.background_pdf.clone(), page0.background) {
            let also = doc.pages.get(1).and_then(|next| next.background);
            rows.push((
                "pdftoppm (page 1, +next)".into(),
                time(&mut || {
                    doc.pdf_page_with(&pdf, number, also).expect("background");
                }),
            ));
        } else {
            rows.push(("pdftoppm (page 1, +next)".into(), Duration::ZERO));
        }
        let mut composed = None;
        rows.push((
            "svg build".into(),
            time(&mut || composed = Some(doc.compose(0, false).expect("compose"))),
        ));
        let svg = composed.expect("composed").svg;
        let mut options = resvg::usvg::Options::default();
        if svg.contains("<text") {
            options.fontdb_mut().load_system_fonts();
        }
        let mut tree = None;
        rows.push((
            "usvg parse".into(),
            time(&mut || tree = Some(resvg::usvg::Tree::from_str(&svg, &options).expect("tree"))),
        ));
        let size = tree.expect("tree").size();
        rows.push((
            format!("svg build ({} KiB)", svg.len() / 1024),
            Duration::ZERO,
        ));
        rows.pop();

        // --- What the user waits for, on a document opened from scratch. ---
        let doc = bundle_document(&bundle, "Benchmark");
        let mut first_paint = None;
        let open = time(&mut || first_paint = Some(doc.preview(0).expect("first paint")));
        let first_paint = first_paint.expect("first paint");
        rows.push((
            format!("OPEN -> first paint @{}", Document::FAST_OVERVIEW),
            open,
        ));
        rows.push((
            "detail overview @1800".into(),
            time(&mut || {
                doc.overview(0, 1800).expect("detail");
            }),
        ));

        let key = |page, column, row| TileKey {
            document_key: doc.document_key.clone(),
            page,
            scale_milli: 1000,
            column,
            row,
        };
        let page = doc.page(0).expect("page");
        rows.push((
            "first tile".into(),
            time(&mut || {
                doc.tile_from(&page, key(0, 0, 0)).expect("tile");
            }),
        ));
        rows.push((
            "subsequent tile".into(),
            time(&mut || {
                doc.tile_from(&page, key(0, 1, 1)).expect("tile");
            }),
        ));
        let columns = (size.width() / TILE_SIZE as f32).ceil() as u32;
        let tile_rows = (size.height() / TILE_SIZE as f32).ceil() as u32;
        let visible: Vec<_> = (0..tile_rows.min(3))
            .flat_map(|r| (0..columns.min(3)).map(move |c| (c, r)))
            .collect();
        let serial = time(&mut || {
            for &(c, r) in &visible {
                doc.tile_from(&page, key(0, c, r)).expect("tile");
            }
        });
        rows.push((format!("{} visible tiles, serial", visible.len()), serial));
        // The same batch across the worker pool, as the app renders it now.
        let workers = std::thread::available_parallelism()
            .map(|count| count.get())
            .unwrap_or(1)
            .clamp(1, crate::service::MAX_TILE_WORKERS);
        let parallel = time(&mut || {
            let shared = &doc;
            let page = &page;
            std::thread::scope(|scope| {
                for chunk in visible.chunks(visible.len().div_ceil(workers)) {
                    scope.spawn(move || {
                        for &(c, r) in chunk {
                            shared.tile_from(page, key(0, c, r)).expect("tile");
                        }
                    });
                }
            });
        });
        rows.push((
            format!("{} visible tiles, {workers} workers", visible.len()),
            parallel,
        ));

        if doc.pages.len() > 1 {
            drop(page);
            rows.push((
                "PAGE 1 -> 2, first paint".into(),
                time(&mut || {
                    doc.preview(1).expect("page 2");
                }),
            ));
            rows.push((
                "page 2 first tile".into(),
                time(&mut || {
                    doc.tile(key(1, 0, 0)).expect("tile");
                }),
            ));
            // Page 1 is still in the LRU cache, so going back repaints it.
            assert!(doc.is_page_cached(0), "page 1 must still be cached");
            rows.push((
                "PAGE 2 -> 1 (cached), first paint".into(),
                time(&mut || {
                    doc.preview(0).expect("back");
                }),
            ));
            // With prefetch, the page after the one shown is already rendered.
            let prefetched = bundle_document(&bundle, "Benchmark");
            prefetched.preview(0).expect("open");
            prefetched.prefetch_page(1).expect("prefetch");
            rows.push((
                "PAGE 1 -> 2 (prefetched), first paint".into(),
                time(&mut || {
                    prefetched.preview(1).expect("page 2");
                }),
            ));
        }

        println!(
            "\n=== {label} ({} pages, canvas {:.0}x{:.0}, first paint {}x{}) ===",
            doc.pages.len(),
            size.width(),
            size.height(),
            first_paint.width,
            first_paint.height,
        );
        for (name, elapsed) in &rows {
            println!("{name:<38}{:>9.1} ms", elapsed.as_secs_f64() * 1000.0);
        }
        println!();
    }

    /// Render every page of a real bundle to PNG through the viewer's tile
    /// path and print its warnings. Skipped unless `REMARKABLE_PAGES_BUNDLE`
    /// points at a bundle directory (the pulled document, including any
    /// `<document>/<page>/` image files); the private sample is never part of
    /// this repository. `REMARKABLE_PAGES_SCALE` is the zoom in thousandths.
    ///
    /// ```sh
    /// REMARKABLE_PAGES_BUNDLE=/path/to/bundle REMARKABLE_PAGES_OUT=/tmp/pages \
    ///   REMARKABLE_PAGES_SCALE=500 \
    ///   cargo test -p turborm --lib renders_every_page -- --nocapture
    /// ```
    #[test]
    fn renders_every_page_of_a_bundle_when_one_is_supplied() {
        let Ok(source) = std::env::var("REMARKABLE_PAGES_BUNDLE") else {
            return;
        };
        let out = PathBuf::from(
            std::env::var("REMARKABLE_PAGES_OUT")
                .unwrap_or_else(|_| "/tmp/remarkable-pages".into()),
        );
        std::fs::create_dir_all(&out).unwrap();
        let scale_milli = std::env::var("REMARKABLE_PAGES_SCALE")
            .ok()
            .and_then(|value| value.parse::<u32>().ok())
            .unwrap_or(500);
        let doc = bundle_document(Path::new(&source), "Pages sample");
        for index in 0..doc.pages.len() {
            let page = doc.page(index).unwrap();
            let scale = scale_milli as f32 / 1000.0;
            let width = (page.size().width() * scale).ceil() as u32;
            let height = (page.size().height() * scale).ceil() as u32;
            let mut sheet = image::RgbaImage::new(width, height);
            for row in 0..height.div_ceil(TILE_SIZE as u32) {
                for column in 0..width.div_ceil(TILE_SIZE as u32) {
                    let tile = doc
                        .tile_from(
                            &page,
                            TileKey {
                                document_key: doc.document_key.clone(),
                                page: index,
                                scale_milli,
                                column,
                                row,
                            },
                        )
                        .unwrap();
                    let pixels = image::RgbaImage::from_raw(
                        tile.width as u32,
                        tile.height as u32,
                        tile.rgba,
                    )
                    .unwrap();
                    image::imageops::replace(
                        &mut sheet,
                        &pixels,
                        i64::from(column) * TILE_SIZE as i64,
                        i64::from(row) * TILE_SIZE as i64,
                    );
                }
            }
            let path = out.join(format!("page-{:03}.png", index + 1));
            sheet.save(&path).unwrap();
            println!(
                "page {} of {}: {} ({width}x{height}, canvas {:.0}x{:.0})",
                index + 1,
                doc.pages.len(),
                path.display(),
                page.size().width(),
                page.size().height()
            );
            for warning in page.warnings() {
                println!("  {warning}");
            }
        }
    }

    /// Copy a bundle directory into a private temporary document.
    fn bundle_document(source: &Path, title: &str) -> Document {
        let id = std::fs::read_dir(source)
            .expect("bundle directory")
            .flatten()
            .find_map(|entry| {
                let path = entry.path();
                (path.extension()? == "content")
                    .then(|| path.file_stem()?.to_str().map(str::to_owned))
                    .flatten()
            })
            .expect("bundle needs a .content file");
        let directory = tempfile::tempdir().unwrap();
        let bundle = directory.path().join("bundle");
        std::fs::create_dir_all(bundle.join(&id)).unwrap();
        for entry in walk(source) {
            let relative = entry.strip_prefix(source).unwrap();
            let target = bundle.join(relative);
            std::fs::create_dir_all(target.parent().unwrap()).unwrap();
            std::fs::copy(&entry, &target).unwrap();
        }
        Document::load(directory, id, title.into()).unwrap()
    }

    fn walk(root: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(directory) = stack.pop() {
            for entry in std::fs::read_dir(&directory)
                .into_iter()
                .flatten()
                .flatten()
            {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else {
                    out.push(path);
                }
            }
        }
        out
    }

    #[test]
    fn ppm_rasters_decode_to_opaque_pixels_and_malformed_ones_are_refused() {
        let mut ppm = b"P6\n2 1\n255\n".to_vec();
        ppm.extend([255, 0, 0, 0, 0, 255]);
        let pixmap = decode_raster(&ppm).unwrap();
        assert_eq!((pixmap.width(), pixmap.height()), (2, 1));
        assert_eq!(pixmap.data(), &[255, 0, 0, 255, 0, 0, 255, 255]);
        assert!(decode_raster(&ppm[..ppm.len() - 1]).is_err(), "truncated");
        assert!(decode_raster(b"P6\n2 1\n65535\n\0\0\0\0\0\0").is_err());
        assert!(decode_raster(b"P6\n# comment\n1 1\n255\n\0\0\0").is_err());
        // The PNG an exported SVG embeds holds the same pixels.
        let png = png_from_pixmap(&pixmap).unwrap();
        assert_eq!(decode_raster(&png).unwrap().data(), pixmap.data());
    }

    /// A long page shown at full width, or any page zoomed in, renders its
    /// PDF background again at the tile's resolution instead of enlarging the
    /// 1800-pixel raster: one-point stripes stay black and white.
    #[test]
    fn zoomed_in_tiles_render_the_pdf_background_at_their_own_resolution() {
        let (width, height) = (200, 2000);
        let mut stripes = String::from("0 0 0 rg\n");
        for x in (0..width).step_by(2) {
            stripes.push_str(&format!("{x} 0 1 {height} re\n"));
        }
        stripes.push_str("f\n");
        let doc =
            pdf_document_with_content(&scene(&[(0.0, 100.0)]), (width, height), &[], &stripes);
        let page = doc.page(0).unwrap();
        let (stored, rect) = page.background.as_ref().unwrap();
        assert!(
            rect.width > stored.width() as f32 * 2.0,
            "the stored raster is coarse"
        );
        let tile = |scale_milli, row| {
            doc.tile_from(
                &page,
                TileKey {
                    document_key: doc.document_key.clone(),
                    page: 0,
                    scale_milli,
                    column: 0,
                    row,
                },
            )
            .unwrap()
        };
        // Share of the background's pixels that are near black and near white.
        let contrast = |tile: &Tile| {
            let left = rect.x.max(0.0) as usize;
            let right = ((rect.x + rect.width) as usize).min(tile.width);
            let mut dark = 0;
            let mut light = 0;
            let mut all = 0;
            for y in 0..tile.height {
                for x in left..right {
                    let red = tile.rgba[(y * tile.width + x) * 4];
                    dark += usize::from(red < 40);
                    light += usize::from(red > 215);
                    all += 1;
                }
            }
            (dark as f32 / all as f32, light as f32 / all as f32)
        };

        // Far enough out, the stored raster is fine enough and no band is made.
        let small = (stored.width() as f32 / rect.width * 1000.0) as u32;
        tile(small, 0);
        assert!(doc.bands.lock().unwrap().is_empty());

        let sharp = tile(1000, 1);
        let (dark, light) = contrast(&sharp);
        assert!(dark > 0.3 && light > 0.3, "{dark} dark, {light} light");
        assert!(!doc.bands.lock().unwrap().is_empty());
        // The same tile again comes from the band cache, identical.
        let bands = doc.bands.lock().unwrap().len();
        assert_eq!(tile(1000, 1).rgba, sharp.rgba);
        assert_eq!(doc.bands.lock().unwrap().len(), bands);
    }
}
