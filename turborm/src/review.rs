//! `turborm review`: send a PDF to the tablet for handwritten review and bring
//! the annotations back as images and text.
//!
//! Everything in this module is offline: argument handling, the saved upload
//! state, the sync check and the rendering of a pulled bundle. The `turborm`
//! executable's `review` subcommand (`src/review_cli.rs`) supplies the cloud
//! session and only ever adds one document or reads.
use crate::renderer::{Document, Preview};
use anyhow::{anyhow, ensure, Context, Result};
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};
use turborm_core::{job, library::Entry, notebook};

/// The top-level folder a review document goes into when it exists.
pub const DEFAULT_FOLDER: &str = "Claude Review";
/// Default longest side of fetched page images, in pixels.
pub const DEFAULT_LONGEST_SIDE: u32 = 2200;
/// Most pages one fetch renders.
pub const MAX_FETCH_PAGES: usize = 500;

#[derive(Parser, Debug)]
#[command(
    name = "turborm review",
    bin_name = "turborm review",
    version,
    about = "Send a PDF to your reMarkable for handwritten review, then fetch the annotations",
    long_about = "Send a PDF to your reMarkable for handwritten review, then fetch the annotations.\n\n\
`send` only ever adds: one new document, plus the folders named by --folder when they are \
missing. It never changes, moves or deletes anything. \
`status` and `fetch` are read-only. Pair first with the TurboRM app (`turborm`)."
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: ReviewCommand,
}

#[derive(Subcommand, Debug, PartialEq)]
pub enum ReviewCommand {
    /// Upload a PDF as a NEW document. Prints the saved upload state as JSON.
    Send {
        /// The PDF to upload.
        #[arg(value_parser = pdf_path)]
        file: PathBuf,
        /// Document title shown on the tablet.
        #[arg(long, value_parser = title)]
        title: String,
        /// Folder path to upload into, such as "Claude Review/TurboRM". Each
        /// existing folder is reused (matched without case) and the missing
        /// ones are created. Without this option the document goes into an
        /// existing top-level "Claude Review" folder, or the library root.
        #[arg(long, value_parser = folder_path)]
        folder: Option<String>,
        /// The file the PDF was built from, recorded in the state for later edits.
        #[arg(long)]
        source: Option<PathBuf>,
        /// Where upload states are kept (default ~/.local/state/turborm-review).
        #[arg(long)]
        state_dir: Option<PathBuf>,
    },
    /// Read-only: whether the document changed in the cloud since it was sent.
    Status {
        /// Document ID printed by `send`.
        #[arg(value_parser = document_id)]
        id: String,
        /// Where upload states are kept (default ~/.local/state/turborm-review).
        #[arg(long)]
        state_dir: Option<PathBuf>,
    },
    /// Read-only: download the document and render every page and its
    /// annotations into a new directory.
    Fetch {
        /// Document ID printed by `send`.
        #[arg(value_parser = document_id)]
        id: String,
        /// New output directory; an existing one is never overwritten.
        #[arg(long)]
        out: PathBuf,
        /// Longest side of each page image in pixels (400 to 4000).
        #[arg(long, default_value_t = DEFAULT_LONGEST_SIDE,
              value_parser = clap::value_parser!(u32).range(400..=Document::MAX_STILL as i64))]
        longest_side: u32,
        /// Where upload states are kept (default ~/.local/state/turborm-review).
        #[arg(long)]
        state_dir: Option<PathBuf>,
    },
}

fn pdf_path(value: &str) -> std::result::Result<PathBuf, String> {
    let path = PathBuf::from(value);
    if path
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("pdf"))
    {
        Ok(path)
    } else {
        Err("the review document must be a .pdf file".into())
    }
}

fn folder_path(value: &str) -> std::result::Result<String, String> {
    turborm_core::folders::split_path(value)
        .map(|segments| segments.join("/"))
        .map_err(|error| error.to_string())
}

fn title(value: &str) -> std::result::Result<String, String> {
    if value.trim().is_empty() || value.chars().count() > 250 || value.chars().any(char::is_control)
    {
        Err("the title must have 1 to 250 characters and no control characters".into())
    } else {
        Ok(value.to_owned())
    }
}

/// A canonical, lowercase, non-nil document UUID.
pub fn document_id(value: &str) -> std::result::Result<String, String> {
    match uuid::Uuid::parse_str(value) {
        Ok(id) if !id.is_nil() && id.to_string() == value => Ok(value.to_owned()),
        _ => Err("expected a document ID such as 0f8fad5b-d9cb-469f-a165-70867728950e".into()),
    }
}

/// What `send` records about one upload, and prints.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct UploadState {
    pub id: String,
    pub title: String,
    /// Destination folder UUID; empty for the library root.
    pub parent: String,
    /// Destination folder name; absent for the library root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder: Option<String>,
    pub uploaded_at: String,
    /// The document's hash in the cloud root right after the upload.
    pub document_hash: String,
    /// Absolute path of the file the PDF was built from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<PathBuf>,
    /// The upload journal, for `turborm-cli verify`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job: Option<PathBuf>,
}

/// `$XDG_STATE_HOME/turborm-review`, else `~/.local/state/turborm-review`.
pub fn default_state_dir() -> Result<PathBuf> {
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))
        .map(|base| base.join("turborm-review"))
        .ok_or_else(|| anyhow!("XDG_STATE_HOME and HOME are unavailable; use --state-dir"))
}

fn state_path(dir: &Path, id: &str) -> Result<PathBuf> {
    document_id(id).map_err(|error| anyhow!(error))?;
    Ok(dir.join(format!("{id}.json")))
}

/// Save a new upload state as `<dir>/<id>.json`. Never replaces one.
pub fn save_state(dir: &Path, state: &UploadState) -> Result<PathBuf> {
    let path = state_path(dir, &state.id)?;
    private_dir(dir)?;
    let mut file = tempfile::NamedTempFile::new_in(dir)?;
    serde_json::to_writer_pretty(&mut file, state)?;
    file.write_all(b"\n")?;
    file.as_file().sync_all()?;
    file.persist_noclobber(&path)
        .map_err(|_| anyhow!("A review state already exists at {}", path.display()))?;
    Ok(path)
}

/// The saved state of `id`, or `None` when this machine did not send it.
pub fn load_state(dir: &Path, id: &str) -> Result<Option<UploadState>> {
    let path = state_path(dir, id)?;
    if !path.exists() {
        return Ok(None);
    }
    let state: UploadState = serde_json::from_slice(&job::read_bounded(&path, 1024 * 1024)?)
        .with_context(|| format!("Invalid review state {}", path.display()))?;
    ensure!(
        state.id == id,
        "Review state {} names another document",
        path.display()
    );
    Ok(Some(state))
}

fn private_dir(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)?;
    }
    #[cfg(not(unix))]
    fs::create_dir_all(path)?;
    Ok(())
}

/// Whether the tablet has written to the document since it was sent: its
/// cloud hash moved, or it now has handwriting pages (`.rm` files).
pub fn changed_since_upload(uploaded_hash: &str, current_hash: &str, scene_pages: usize) -> bool {
    uploaded_hash != current_hash || scene_pages > 0
}

/// The document as the cloud has it now.
#[derive(Debug, Clone, PartialEq)]
pub struct CloudDocument {
    pub title: String,
    pub parent: String,
    pub deleted: bool,
    /// `lastModified` from the metadata, in milliseconds since the epoch.
    pub last_modified: Option<String>,
    pub hash: String,
    pub page_count: Option<u32>,
    /// Handwriting pages (`.rm` files) the document has.
    pub scene_pages: usize,
}

/// The JSON `status` prints.
pub fn status_json(
    id: &str,
    current: Option<&CloudDocument>,
    saved: Option<&UploadState>,
) -> Value {
    let Some(current) = current else {
        return json!({
            "id": id,
            "exists": false,
            "changed_since_upload": Value::Null,
            "uploaded_hash": saved.map(|state| state.document_hash.clone()),
        });
    };
    json!({
        "id": id,
        "exists": true,
        "deleted": current.deleted,
        "title": current.title,
        "parent": current.parent,
        "last_modified": current.last_modified,
        "last_modified_utc": current.last_modified.as_deref().and_then(millis_to_utc),
        "hash": current.hash,
        "page_count": current.page_count,
        "scene_pages": current.scene_pages,
        "uploaded_at": saved.map(|state| state.uploaded_at.clone()),
        "uploaded_hash": saved.map(|state| state.document_hash.clone()),
        "changed_since_upload": saved
            .map(|state| changed_since_upload(&state.document_hash, &current.hash, current.scene_pages)),
        "state": if saved.is_some() { "saved" } else { "missing: this machine has no record of sending it" },
    })
}

/// The live top-level folder named `name`, ignoring case, if there is one.
/// With several, the oldest ID wins and the count is returned for a note.
pub fn find_folder<'a>(entries: &'a [Entry], name: &str) -> (Option<&'a Entry>, usize) {
    let wanted = name.trim().to_lowercase();
    let mut matches: Vec<&Entry> = entries
        .iter()
        .filter(|entry| {
            entry.kind == "folder"
                && !entry.deleted
                && entry.parent.is_empty()
                && entry.hierarchy_warning.is_none()
                && entry.title.trim().to_lowercase() == wanted
        })
        .collect();
    matches.sort_by(|a, b| a.id.cmp(&b.id));
    (matches.first().copied(), matches.len())
}

/// RFC 3339 UTC, to the second.
pub fn utc_timestamp(time: SystemTime) -> String {
    let seconds = time
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    utc_from_seconds(seconds)
}

fn utc_from_seconds(seconds: u64) -> String {
    let days = (seconds / 86_400) as i64;
    let rest = seconds % 86_400;
    // Howard Hinnant's civil-from-days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rest / 3_600,
        rest / 60 % 60,
        rest % 60
    )
}

/// A metadata `lastModified` (milliseconds as a string) as RFC 3339 UTC.
pub fn millis_to_utc(value: &str) -> Option<String> {
    value
        .parse::<u64>()
        .ok()
        .map(|millis| utc_from_seconds(millis / 1000))
}

/// What the caller knows about the fetched document besides its bundle.
#[derive(Debug, Default)]
pub struct FetchContext {
    pub document_hash: Option<String>,
    pub generation: Option<u64>,
    pub saved: Option<UploadState>,
}

/// Render a pulled bundle into a NEW directory `out`.
///
/// `directory` holds the native files under `bundle/`, exactly as
/// `Library::pull` writes them. For each page N this writes `page-NNN.png`,
/// the complete page with its annotations, and, when the page has any,
/// `page-NNN-ink.png`, the annotations alone on white in the same geometry.
/// `annotations.json` and `summary.md` describe every page. Returns the JSON.
pub fn render_review(
    directory: tempfile::TempDir,
    id: &str,
    longest_side: u32,
    out: &Path,
    context: FetchContext,
) -> Result<Value> {
    ensure!(
        !out.exists(),
        "{} already exists; choose a new output directory",
        out.display()
    );
    let parent = out
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent)?;
    let staging = tempfile::Builder::new()
        .prefix(".turborm-review-")
        .tempdir_in(parent)?;
    let bundle = directory.path().join("bundle");
    let metadata: Value =
        job::read_bounded(&bundle.join(format!("{id}.metadata")), 16 * 1024 * 1024)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or(Value::Null);
    let title = metadata["visibleName"].as_str().unwrap_or(id).to_owned();
    let has_pdf = bundle.join(format!("{id}.pdf")).is_file();
    let scenes = bundle.join(id);
    let document =
        Document::load_for_stills(directory, id.to_owned(), title.clone(), longest_side)?;
    ensure!(
        document.pages.len() <= MAX_FETCH_PAGES,
        "The document has {} pages; fetch renders at most {MAX_FETCH_PAGES}",
        document.pages.len()
    );
    let mut pages = Vec::new();
    for (index, page) in document.pages.iter().enumerate() {
        let number = index + 1;
        let file = format!("page-{number:03}.png");
        let mut warnings = Vec::new();
        let mut notes = Vec::new();
        let mut scene_text = notebook::SceneAnnotations::default();
        let scene = scenes.join(format!("{}.rm", page.id));
        if !page.id.is_empty() && scene.is_file() {
            match job::read_bounded(&scene, 64 * 1024 * 1024)
                .and_then(|bytes| notebook::scene_annotations(&bytes))
            {
                Ok(found) => scene_text = found,
                Err(error) => warnings.push(format!("Scene text could not be read: {error:#}")),
            }
        }
        warnings.append(&mut scene_text.warnings);
        let complete = document
            .render_page(index)
            .and_then(|rendered| {
                for entry in rendered.warnings() {
                    let note = notebook::severity(entry);
                    match note.severity {
                        notebook::Severity::Note => notes.push(note.text),
                        notebook::Severity::Warning => warnings.push(note.text),
                    }
                }
                document.still(&rendered, longest_side)
            })
            .and_then(|image| {
                write_png(&staging.path().join(&file), &image)?;
                Ok(image)
            });
        let complete = match complete {
            Ok(image) => image,
            Err(error) => {
                pages.push(json!({
                    "page": number,
                    "page_id": page.id,
                    "annotated": Value::Null,
                    "error": format!("{error:#}"),
                    "highlights": highlights_json(&scene_text.highlights),
                    "typed_text": scene_text.typed_text,
                    "colors": scene_text.stroke_colors,
                    "warnings": warnings,
                }));
                continue;
            }
        };
        let annotations = document
            .render_annotations(index)
            .and_then(|rendered| document.still(&rendered, longest_side));
        let (ink_pixels, regions, ink_file, ink_image) = match annotations {
            Ok(image) => {
                let (ink_pixels, regions) = ink_regions(&image.rgba, image.width, image.height);
                let annotated = ink_pixels > 0
                    || !scene_text.highlights.is_empty()
                    || !scene_text.typed_text.is_empty();
                let ink_file = if annotated {
                    let name = format!("page-{number:03}-ink.png");
                    write_png(&staging.path().join(&name), &image)?;
                    Some(name)
                } else {
                    None
                };
                (ink_pixels, regions, ink_file, Some(image))
            }
            Err(error) => {
                warnings.push(format!(
                    "The annotation layer could not be drawn: {error:#}"
                ));
                (0, Vec::new(), None, None)
            }
        };
        // A long page is also cut into screen-shaped parts, each readable at
        // full size; marks are listed per part in that part's pixels.
        let mut parts = Vec::new();
        for (k, (top, rows)) in long_page_parts(complete.width, complete.height)
            .into_iter()
            .enumerate()
        {
            let part = k + 1;
            let file = format!("page-{number:03}-part-{part:02}.png");
            write_rows(&staging.path().join(&file), &complete, top, rows)?;
            let inside: Vec<[u32; 4]> = regions
                .iter()
                .filter_map(|region| clip_rows(*region, top, rows))
                .collect();
            let ink_file = match (&ink_image, ink_file.is_some() && !inside.is_empty()) {
                (Some(image), true) => {
                    let name = format!("page-{number:03}-ink-part-{part:02}.png");
                    write_rows(&staging.path().join(&name), image, top, rows)?;
                    Some(name)
                }
                _ => None,
            };
            parts.push(json!({
                "part": part,
                "file": file,
                "ink_file": ink_file,
                "top": top,
                "height": rows,
                "ink_bounds": inside,
            }));
        }
        let annotated = ink_file.is_some()
            || !scene_text.highlights.is_empty()
            || !scene_text.typed_text.is_empty();
        let bbox = regions.iter().copied().reduce(|a, b| {
            let x = a[0].min(b[0]);
            let y = a[1].min(b[1]);
            [
                x,
                y,
                (a[0] + a[2]).max(b[0] + b[2]) - x,
                (a[1] + a[3]).max(b[1] + b[3]) - y,
            ]
        });
        pages.push(json!({
            "page": number,
            "page_id": page.id,
            "file": file,
            "ink_file": ink_file,
            "annotated": annotated,
            "size": [complete.width, complete.height],
            "source_pdf_page": page.background.map(|n| n + 1),
            "inserted_page": has_pdf && page.background.is_none(),
            "highlights": highlights_json(&scene_text.highlights),
            "typed_text": scene_text.typed_text,
            "colors": scene_text.stroke_colors,
            "ink_pixels": ink_pixels,
            "ink_bbox": bbox,
            "ink_bounds": regions,
            "parts": parts,
            "warnings": warnings,
            "notes": notes,
        }));
    }
    let saved = context.saved.as_ref();
    let report = json!({
        "version": 1,
        "document": {
            "id": id,
            "title": title,
            "hash": context.document_hash,
            "generation": context.generation,
            "pages": pages.len(),
            "fetched_at": utc_timestamp(SystemTime::now()),
            "longest_side": longest_side,
            "uploaded_at": saved.map(|state| state.uploaded_at.clone()),
            "uploaded_hash": saved.map(|state| state.document_hash.clone()),
            "source": saved.and_then(|state| state.source.clone()),
        },
        "coordinates": "ink_bbox and ink_bounds are [x, y, width, height] in pixels of page-NNN.png (and of page-NNN-ink.png, which has the same geometry); a part's ink_bounds are in pixels of that part's images, which start at its top row of the page image",
        "pages": pages,
    });
    fs::write(
        staging.path().join("annotations.json"),
        serde_json::to_vec_pretty(&report)?,
    )?;
    fs::write(staging.path().join("summary.md"), summary(&report))?;
    // Reserve the destination atomically; never merge into an existing one.
    fs::create_dir(out)
        .with_context(|| format!("Cannot create the new output directory {}", out.display()))?;
    let mut children = fs::read_dir(staging.path())?.collect::<std::io::Result<Vec<_>>>()?;
    children.sort_by_key(|child| child.file_name());
    for child in children {
        fs::rename(child.path(), out.join(child.file_name()))?;
    }
    Ok(report)
}

fn highlights_json(highlights: &[notebook::Highlight]) -> Value {
    highlights
        .iter()
        .map(|highlight| json!({"text": highlight.text, "color": highlight.color}))
        .collect()
}

fn write_png(path: &Path, image: &Preview) -> Result<()> {
    write_rows(path, image, 0, image.height)
}

/// Write `rows` rows of `image` from row `top` on as a PNG.
fn write_rows(path: &Path, image: &Preview, top: usize, rows: usize) -> Result<()> {
    let stride = image.width * 4;
    let pixels = image
        .rgba
        .get(top * stride..(top + rows) * stride)
        .ok_or_else(|| anyhow::anyhow!("Rows {top}..{} are outside the page", top + rows))?;
    // Pages are painted on opaque white, so dropping alpha loses nothing.
    let rgb: Vec<u8> = pixels
        .as_chunks::<4>()
        .0
        .iter()
        .flat_map(|p| [p[0], p[1], p[2]])
        .collect();
    image::save_buffer_with_format(
        path,
        &rgb,
        image.width as u32,
        rows as u32,
        image::ColorType::Rgb8,
        image::ImageFormat::Png,
    )
    .with_context(|| format!("Could not write {}", path.display()))
}

/// The parts of a long page image, as `(top, rows)`: each as tall as a 3:4
/// page of the same width, overlapping the next by an eighth so a mark on a
/// boundary is whole in at least one part. Empty for an ordinary page.
pub fn long_page_parts(width: usize, height: usize) -> Vec<(usize, usize)> {
    let part = (width * 4 / 3).max(1);
    if height <= part + part / 20 {
        return Vec::new();
    }
    let step = part - part / 8;
    let mut parts = Vec::new();
    let mut top = 0;
    while top + part < height {
        parts.push((top, part));
        top += step;
    }
    parts.push((height - part, part));
    parts
}

/// The part of `region` within rows `top..top + rows`, in the part's pixels.
fn clip_rows(region: [u32; 4], top: usize, rows: usize) -> Option<[u32; 4]> {
    let [x, y, width, height] = region;
    let first = y.max(top as u32);
    let end = (y + height).min((top + rows) as u32);
    (end > first).then(|| [x, first - top as u32, width, end - first])
}

/// Count the drawn pixels of an annotations-only image (anything not near
/// white) and group them into regions: pixels closer than about 2% of the
/// longest side join one region, so a handwritten word or line becomes one
/// box. Regions are `[x, y, width, height]` in reading order.
pub fn ink_regions(rgba: &[u8], width: usize, height: usize) -> (usize, Vec<[u32; 4]>) {
    const MAX_REGIONS: usize = 200;
    if width == 0 || height == 0 || rgba.len() < width * height * 4 {
        return (0, Vec::new());
    }
    // Cells follow a 3:4 page of this width, so a long page groups its
    // handwriting as finely as an ordinary one.
    let cell = (width.max(height.min(width * 4 / 3)) / 220).max(4);
    let gap = 4usize;
    let (columns, rows) = (width.div_ceil(cell), height.div_ceil(cell));
    // Tight pixel bounds of the ink in each cell.
    let mut cells: Vec<Option<[usize; 4]>> = vec![None; columns * rows];
    let mut count = 0usize;
    for y in 0..height {
        for x in 0..width {
            let p = &rgba[(y * width + x) * 4..][..3];
            if p[0].min(p[1]).min(p[2]) >= 235 {
                continue;
            }
            count += 1;
            let slot = &mut cells[(y / cell) * columns + x / cell];
            *slot = Some(match *slot {
                Some([x0, y0, x1, y1]) => [x0.min(x), y0.min(y), x1.max(x), y1.max(y)],
                None => [x, y, x, y],
            });
        }
    }
    let mut seen = vec![false; columns * rows];
    let mut regions = Vec::new();
    for start in 0..cells.len() {
        if seen[start] || cells[start].is_none() {
            continue;
        }
        seen[start] = true;
        let mut stack = vec![start];
        let mut bounds = cells[start].expect("checked cell");
        while let Some(at) = stack.pop() {
            let [x0, y0, x1, y1] = cells[at].expect("queued cells have ink");
            bounds = [
                bounds[0].min(x0),
                bounds[1].min(y0),
                bounds[2].max(x1),
                bounds[3].max(y1),
            ];
            let (column, row) = (at % columns, at / columns);
            for r in row.saturating_sub(gap)..(row + gap + 1).min(rows) {
                for c in column.saturating_sub(gap)..(column + gap + 1).min(columns) {
                    let next = r * columns + c;
                    if !seen[next] && cells[next].is_some() {
                        seen[next] = true;
                        stack.push(next);
                    }
                }
            }
        }
        regions.push([
            bounds[0] as u32,
            bounds[1] as u32,
            (bounds[2] - bounds[0] + 1) as u32,
            (bounds[3] - bounds[1] + 1) as u32,
        ]);
    }
    if regions.len() > MAX_REGIONS {
        regions.sort_by_key(|r| std::cmp::Reverse(u64::from(r[2]) * u64::from(r[3])));
        regions.truncate(MAX_REGIONS);
    }
    regions.sort_by_key(|r| (r[1], r[0]));
    (count, regions)
}

/// The Markdown overview of a fetch, in page order.
pub fn summary(report: &Value) -> String {
    let document = &report["document"];
    let pages = report["pages"].as_array().cloned().unwrap_or_default();
    let annotated: Vec<&Value> = pages
        .iter()
        .filter(|page| page["annotated"] == true)
        .collect();
    let mut out = format!(
        "# Review annotations: {}\n\n",
        document["title"].as_str().unwrap_or("")
    );
    out.push_str(&format!(
        "- Document: `{}`\n- Fetched: {}; {} pages, {} with annotations\n",
        document["id"].as_str().unwrap_or(""),
        document["fetched_at"].as_str().unwrap_or(""),
        pages.len(),
        annotated.len()
    ));
    if let Some(source) = document["source"].as_str() {
        out.push_str(&format!("- Source: `{source}`\n"));
    }
    out.push_str(
        "- Images: `page-NNN.png` is the page with its annotations; `page-NNN-ink.png` is the annotations alone, same geometry. A long page is also cut into screen-sized parts, `page-NNN-part-KK.png` and `page-NNN-ink-part-KK.png`\n",
    );
    for page in &pages {
        let number = page["page"].as_u64().unwrap_or(0);
        if let Some(error) = page["error"].as_str() {
            out.push_str(&format!(
                "\n## Page {number}: could not be rendered\n\n{error}\n"
            ));
            continue;
        }
        if page["annotated"] != true {
            continue;
        }
        let inserted = if page["inserted_page"] == true {
            " (a page added on the tablet)"
        } else {
            ""
        };
        out.push_str(&format!("\n## Page {number}{inserted}\n\n"));
        let regions = page["ink_bounds"].as_array().map_or(0, Vec::len);
        if regions > 0 {
            out.push_str(&format!(
                "- Marks: {regions} region{} (see `{}`)\n",
                if regions == 1 { "" } else { "s" },
                page["ink_file"].as_str().unwrap_or("")
            ));
        }
        let parts = page["parts"].as_array().cloned().unwrap_or_default();
        if !parts.is_empty() {
            let marked: Vec<String> = parts
                .iter()
                .filter(|part| part["ink_file"].is_string())
                .map(|part| part["part"].to_string())
                .collect();
            out.push_str(&format!(
                "- Long page in {} parts (`page-{number:03}-part-NN.png`); parts with marks: {}\n",
                parts.len(),
                if marked.is_empty() {
                    "none".to_owned()
                } else {
                    marked.join(", ")
                }
            ));
        }
        if let Some(colors) = page["colors"].as_object().filter(|c| !c.is_empty()) {
            let list: Vec<String> = colors
                .iter()
                .map(|(name, count)| format!("{name} ×{count}"))
                .collect();
            out.push_str(&format!("- Stroke colours: {}\n", list.join(", ")));
        }
        let highlights = page["highlights"].as_array().cloned().unwrap_or_default();
        if !highlights.is_empty() {
            out.push_str("- Highlighted:\n");
            for item in highlights {
                out.push_str(&format!(
                    "  - \"{}\" ({})\n",
                    item["text"].as_str().unwrap_or(""),
                    item["color"].as_str().unwrap_or("unknown colour")
                ));
            }
        }
        let typed = page["typed_text"].as_array().cloned().unwrap_or_default();
        if !typed.is_empty() {
            out.push_str("- Typed text:\n");
            for item in typed {
                out.push_str(&format!("  - \"{}\"\n", item.as_str().unwrap_or("")));
            }
        }
        for warning in page["warnings"].as_array().into_iter().flatten() {
            out.push_str(&format!("- Warning: {}\n", warning.as_str().unwrap_or("")));
        }
    }
    let quiet: Vec<String> = pages
        .iter()
        .filter(|page| page["annotated"] == false)
        .map(|page| page["page"].to_string())
        .collect();
    if annotated.is_empty() {
        out.push_str("\nNo page has annotations yet.\n");
    }
    if !quiet.is_empty() {
        out.push_str(&format!(
            "\n## Pages without annotations\n\n{}\n",
            quiet.join(", ")
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use lopdf::{dictionary, Object, Stream};

    const ID: &str = "3f2c6d1e-8a4b-4c5d-9e6f-7a8b9c0d1e2f";

    fn parse(args: &[&str]) -> std::result::Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("turborm review").chain(args.iter().copied()))
    }

    #[test]
    fn arguments_are_parsed_and_validated() {
        let cli = parse(&["send", "plan.pdf", "--title", "Plan (review)"]).unwrap();
        assert_eq!(
            cli.command,
            ReviewCommand::Send {
                file: "plan.pdf".into(),
                title: "Plan (review)".into(),
                folder: None,
                source: None,
                state_dir: None,
            }
        );
        let cli = parse(&[
            "send",
            "a.PDF",
            "--title",
            "T",
            "--folder",
            " Claude Review / TurboRM/",
            "--source",
            "/x/plan.md",
            "--state-dir",
            "/tmp/s",
        ])
        .unwrap();
        assert!(
            matches!(cli.command, ReviewCommand::Send { ref folder, ref source, .. }
            if folder.as_deref() == Some("Claude Review/TurboRM")
                && source.as_deref() == Some(Path::new("/x/plan.md")))
        );
        // A folder path is validated before anything is sent.
        assert!(parse(&["send", "a.pdf", "--title", "T", "--folder", "A//B"]).is_err());
        assert!(parse(&["send", "a.pdf", "--title", "T", "--folder", "/"]).is_err());
        // Only PDFs, and a title is required and must be printable.
        assert!(parse(&["send", "plan.md", "--title", "T"]).is_err());
        assert!(parse(&["send", "plan.pdf"]).is_err());
        assert!(parse(&["send", "plan.pdf", "--title", "  "]).is_err());
        assert!(parse(&["send", "plan.pdf", "--title", "a\nb"]).is_err());

        let cli = parse(&["fetch", ID, "--out", "/tmp/r"]).unwrap();
        assert!(matches!(
            cli.command,
            ReviewCommand::Fetch {
                longest_side: DEFAULT_LONGEST_SIDE,
                ..
            }
        ));
        let cli = parse(&["fetch", ID, "--out", "r", "--longest-side", "4000"]).unwrap();
        assert!(matches!(
            cli.command,
            ReviewCommand::Fetch {
                longest_side: 4000,
                ..
            }
        ));
        assert!(parse(&["fetch", ID, "--out", "r", "--longest-side", "4001"]).is_err());
        assert!(parse(&["fetch", ID, "--out", "r", "--longest-side", "100"]).is_err());
        assert!(parse(&["fetch", ID]).is_err(), "--out is required");
        // IDs must be canonical document UUIDs.
        assert!(parse(&["status", ID]).is_ok());
        assert!(parse(&["status", &ID.to_uppercase()]).is_err());
        assert!(parse(&["status", "../etc/passwd"]).is_err());
        assert!(parse(&["status", "00000000-0000-0000-0000-000000000000"]).is_err());
        assert!(
            parse(&["delete", ID]).is_err(),
            "there is no destructive command"
        );
    }

    fn state() -> UploadState {
        UploadState {
            id: ID.into(),
            title: "Plan (review 2026-09-23)".into(),
            parent: String::new(),
            folder: None,
            uploaded_at: "2026-09-23T10:00:00Z".into(),
            document_hash: "a".repeat(64),
            source: Some("/home/u/plan.md".into()),
            job: None,
        }
    }

    #[test]
    fn the_state_file_round_trips_and_is_never_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let states = dir.path().join("nested/state");
        assert_eq!(load_state(&states, ID).unwrap(), None);
        let path = save_state(&states, &state()).unwrap();
        assert_eq!(path, states.join(format!("{ID}.json")));
        assert_eq!(load_state(&states, ID).unwrap(), Some(state()));
        let saved: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        for key in [
            "id",
            "title",
            "parent",
            "uploaded_at",
            "document_hash",
            "source",
        ] {
            assert!(saved.get(key).is_some(), "{key} missing from {saved}");
        }
        assert!(saved.get("folder").is_none(), "root uploads name no folder");
        assert!(save_state(&states, &state()).is_err());
        assert!(load_state(&states, "not-an-id").is_err());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&states).unwrap().permissions().mode();
            assert_eq!(mode & 0o077, 0, "state directory is private");
        }
    }

    #[test]
    fn a_change_is_a_new_hash_or_any_handwriting_page() {
        let old = "a".repeat(64);
        let new = "b".repeat(64);
        assert!(!changed_since_upload(&old, &old, 0));
        assert!(changed_since_upload(&old, &new, 0));
        assert!(changed_since_upload(&old, &old, 1));
        let current = CloudDocument {
            title: "Plan".into(),
            parent: String::new(),
            deleted: false,
            last_modified: Some("1790000000000".into()),
            hash: new.clone(),
            page_count: Some(2),
            scene_pages: 0,
        };
        let status = status_json(ID, Some(&current), Some(&state()));
        assert_eq!(status["exists"], true);
        assert_eq!(status["changed_since_upload"], true);
        assert_eq!(status["hash"], new);
        assert_eq!(status["last_modified_utc"], "2026-09-21T14:13:20Z");
        let unchanged = CloudDocument {
            hash: "a".repeat(64),
            ..current.clone()
        };
        assert_eq!(
            status_json(ID, Some(&unchanged), Some(&state()))["changed_since_upload"],
            false
        );
        assert_eq!(
            status_json(ID, Some(&current), None)["changed_since_upload"],
            Value::Null
        );
        let gone = status_json(ID, None, Some(&state()));
        assert_eq!(gone["exists"], false);
    }

    #[test]
    fn timestamps_are_utc_rfc3339() {
        assert_eq!(utc_from_seconds(0), "1970-01-01T00:00:00Z");
        assert_eq!(utc_from_seconds(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(utc_from_seconds(1_790_000_000), "2026-09-21T14:13:20Z");
        assert_eq!(millis_to_utc("0"), Some("1970-01-01T00:00:00Z".into()));
        assert_eq!(millis_to_utc("soon"), None);
    }

    fn entry(id: &str, title: &str, kind: &str, parent: &str, deleted: bool) -> Entry {
        Entry {
            id: id.into(),
            title: title.into(),
            kind: kind.into(),
            parent: parent.into(),
            deleted,
            pinned: false,
            modified: None,
            files: 0,
            size: 0,
            path: title.into(),
            hierarchy_warning: None,
            file_type: None,
            page_count: None,
        }
    }

    #[test]
    fn the_review_folder_is_a_live_top_level_folder_matched_without_case() {
        let entries = [
            entry("b", "Claude Review", "document", "", false),
            entry("c", "claude review", "folder", "x", false),
            entry("d", "CLAUDE REVIEW", "folder", "", true),
            entry("e", "Other", "folder", "", false),
        ];
        let (none, count) = find_folder(&entries, DEFAULT_FOLDER);
        assert!(none.is_none() && count == 0);
        let mut entries = entries.to_vec();
        entries.push(entry("f", " claude REVIEW ", "folder", "", false));
        entries.push(entry("a", "Claude review", "folder", "", false));
        let (found, count) = find_folder(&entries, DEFAULT_FOLDER);
        assert_eq!((found.map(|e| e.id.as_str()), count), (Some("a"), 2));
        assert_eq!(
            find_folder(&entries, "other").0.map(|e| e.id.as_str()),
            Some("e")
        );
    }

    #[test]
    fn ink_regions_group_nearby_marks_and_separate_distant_ones() {
        let (width, height) = (400usize, 600usize);
        let mut rgba = vec![255u8; width * height * 4];
        let mut dot = |x: usize, y: usize| {
            let at = (y * width + x) * 4;
            rgba[at..at + 3].copy_from_slice(&[20, 20, 20]);
        };
        // Two strokes a few pixels apart form one word; one far below is another.
        for x in 50..80 {
            dot(x, 100);
        }
        for x in 90..120 {
            dot(x, 102);
        }
        for x in 200..260 {
            dot(x, 500);
        }
        let (pixels, regions) = ink_regions(&rgba, width, height);
        assert_eq!(pixels, 120);
        assert_eq!(regions, vec![[50, 100, 70, 3], [200, 500, 60, 1]]);
        assert_eq!(
            ink_regions(&vec![255; width * height * 4], width, height),
            (0, vec![])
        );
    }

    /// A v6 scene: one pen stroke in the root group and one text highlight.
    fn review_scene(highlight: &str) -> Vec<u8> {
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
        let mut packed = Vec::new();
        for (x, y) in [(-300.0f32, 400.0f32), (300.0, 420.0)] {
            packed.extend(x.to_le_bytes());
            packed.extend(y.to_le_bytes());
            packed.extend(0u16.to_le_bytes());
            packed.extend(16u16.to_le_bytes());
            packed.extend([0, 255]);
        }
        let mut line = vec![3];
        line.extend(field(0x14, &15u32.to_le_bytes()));
        line.extend(field(0x24, &7u32.to_le_bytes()));
        line.extend(field(0x38, &2.0f64.to_le_bytes()));
        line.extend(field(0x5c, &packed));
        let mut stroke = field(0x1f, &[0, 1]);
        stroke.extend(field(0x2f, &[1, 4]));
        stroke.extend(field(0x6c, &line));

        let mut text = vec![highlight.len() as u8, 1];
        text.extend(highlight.as_bytes());
        let mut rects = vec![1u8];
        for value in [-200.0f64, 800.0, 400.0, 45.0] {
            rects.extend(value.to_le_bytes());
        }
        let mut glyph = vec![1];
        glyph.extend(field(0x44, &3u32.to_le_bytes()));
        glyph.extend(field(0x5c, &text));
        glyph.extend(field(0x6c, &rects));
        let mut highlight = field(0x1f, &[0, 1]);
        highlight.extend(field(0x2f, &[1, 60]));
        highlight.extend(field(0x6c, &glyph));

        let mut scene = b"reMarkable .lines file, version=6          ".to_vec();
        scene.extend(block([0, 1, 2, 5], &stroke));
        scene.extend(block([0, 1, 1, 3], &highlight));
        scene
    }

    /// A pulled bundle: a two-page PDF whose first page carries a stroke and
    /// a highlight, an untouched second page, and a page added on the tablet
    /// between them.
    fn pulled_bundle() -> tempfile::TempDir {
        let directory = tempfile::tempdir().unwrap();
        let bundle = directory.path().join("bundle");
        let pages = [
            "00000000-0000-4000-8000-000000000001",
            "00000000-0000-4000-8000-000000000002",
            "00000000-0000-4000-8000-000000000003",
        ];
        fs::create_dir_all(bundle.join(ID)).unwrap();
        let mut pdf = lopdf::Document::with_version("1.5");
        let tree = pdf.new_object_id();
        let mut kids = Vec::new();
        for _ in 0..2 {
            let content = pdf.add_object(Stream::new(
                dictionary! {},
                b"0 0 1 rg 50 600 400 40 re f".to_vec(),
            ));
            let page = pdf.add_object(dictionary! {"Type"=>"Page", "Parent"=>tree,
            "MediaBox"=>vec![0.into(),0.into(),510.into(),680.into()], "Contents"=>content});
            kids.push(Object::Reference(page));
        }
        pdf.objects.insert(
            tree,
            dictionary! {"Type"=>"Pages", "Kids"=>kids, "Count"=>2}.into(),
        );
        let catalog = pdf.add_object(dictionary! {"Type"=>"Catalog", "Pages"=>tree});
        pdf.trailer.set("Root", catalog);
        pdf.save(bundle.join(format!("{ID}.pdf"))).unwrap();
        fs::write(
            bundle.join(format!("{ID}.content")),
            json!({"formatVersion":1,"fileType":"pdf","pages":pages,
                   "redirectionPageMap":[0,-1,1],"originalPageCount":2})
            .to_string(),
        )
        .unwrap();
        fs::write(
            bundle.join(format!("{ID}.metadata")),
            json!({"visibleName":"Plan (review)","type":"DocumentType","parent":""}).to_string(),
        )
        .unwrap();
        fs::write(
            bundle.join(format!("{ID}.pagedata")),
            "Blank\nBlank\nBlank\n",
        )
        .unwrap();
        fs::write(
            bundle.join(ID).join(format!("{}.rm", pages[0])),
            review_scene("Keep this sentence"),
        )
        .unwrap();
        directory
    }

    #[test]
    fn fetch_renders_every_page_and_reports_its_annotations() {
        let output = tempfile::tempdir().unwrap();
        let out = output.path().join("review");
        let report = render_review(
            pulled_bundle(),
            ID,
            800,
            &out,
            FetchContext {
                document_hash: Some("c".repeat(64)),
                generation: Some(7),
                saved: Some(state()),
            },
        )
        .unwrap();
        let pages = report["pages"].as_array().unwrap();
        assert_eq!(pages.len(), 3);
        assert_eq!(report["document"]["title"], "Plan (review)");
        assert_eq!(report["document"]["source"], "/home/u/plan.md");

        let first = &pages[0];
        assert_eq!(first["annotated"], true, "{first}");
        assert_eq!(
            first["highlights"],
            json!([{"text": "Keep this sentence", "color": "yellow"}])
        );
        assert_eq!(first["colors"], json!({"red": 1}));
        assert_eq!(first["source_pdf_page"], 1);
        assert_eq!(first["ink_file"], "page-001-ink.png");
        let regions = first["ink_bounds"].as_array().unwrap();
        assert!(!regions.is_empty());
        let size = first["size"].as_array().unwrap();
        assert_eq!(size.iter().map(|v| v.as_u64().unwrap()).max(), Some(800));

        // The added page renders from the paper, not the PDF; the untouched
        // PDF page renders but has nothing to show on its own.
        assert_eq!(pages[1]["inserted_page"], true);
        assert_eq!(pages[1]["annotated"], false);
        assert_eq!(pages[2]["annotated"], false);
        assert_eq!(pages[2]["source_pdf_page"], 2);
        assert_eq!(pages[2]["ink_file"], Value::Null);

        let mut files: Vec<String> = fs::read_dir(&out)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        files.sort();
        assert_eq!(
            files,
            [
                "annotations.json",
                "page-001-ink.png",
                "page-001.png",
                "page-002.png",
                "page-003.png",
                "summary.md"
            ]
        );
        let saved: Value =
            serde_json::from_slice(&fs::read(out.join("annotations.json")).unwrap()).unwrap();
        assert_eq!(saved["pages"], report["pages"]);

        // The ink image has the page's geometry, the stroke, and no PDF.
        let full = image::open(out.join("page-001.png")).unwrap().to_rgb8();
        let ink = image::open(out.join("page-001-ink.png")).unwrap().to_rgb8();
        assert_eq!(full.dimensions(), ink.dimensions());
        let blue = |image: &image::RgbImage| {
            image
                .pixels()
                .filter(|p| p[0] < 60 && p[1] < 60 && p[2] > 200)
                .count()
        };
        assert!(blue(&full) > 100);
        assert_eq!(blue(&ink), 0);
        let [x, y, w, h] = [0, 1, 2, 3].map(|i| regions[0][i].as_u64().unwrap() as u32);
        let inside = (x..x + w)
            .flat_map(|px| (y..y + h).map(move |py| (px, py)))
            .filter(|&(px, py)| ink.get_pixel(px, py).0.iter().any(|&c| c < 200))
            .count();
        assert!(inside > 0, "the region holds ink");

        let summary = fs::read_to_string(out.join("summary.md")).unwrap();
        assert!(summary.contains("## Page 1\n"), "{summary}");
        assert!(
            summary.contains("\"Keep this sentence\" (yellow)"),
            "{summary}"
        );
        assert!(summary.contains("Stroke colours: red ×1"), "{summary}");
        assert!(
            summary.contains("## Pages without annotations\n\n2, 3"),
            "{summary}"
        );

        // The output directory is never reused.
        assert!(render_review(pulled_bundle(), ID, 800, &out, FetchContext::default()).is_err());
        let leftovers = fs::read_dir(output.path()).unwrap().count();
        assert_eq!(leftovers, 1, "no staging directory is left behind");
    }

    #[test]
    fn a_long_page_is_rendered_by_width_and_cut_into_parts() {
        // One 180 x 1200 mm page with a blue bar near its bottom.
        let directory = tempfile::tempdir().unwrap();
        let bundle = directory.path().join("bundle");
        fs::create_dir_all(bundle.join(ID)).unwrap();
        let mut pdf = lopdf::Document::with_version("1.5");
        let tree = pdf.new_object_id();
        let content = pdf.add_object(Stream::new(
            dictionary! {},
            b"0 0 1 rg 50 100 400 40 re f".to_vec(),
        ));
        let page = pdf.add_object(dictionary! {"Type"=>"Page", "Parent"=>tree,
        "MediaBox"=>vec![0.into(),0.into(),510.into(),3400.into()], "Contents"=>content});
        pdf.objects.insert(
            tree,
            dictionary! {"Type"=>"Pages", "Kids"=>vec![page.into()], "Count"=>1}.into(),
        );
        let catalog = pdf.add_object(dictionary! {"Type"=>"Catalog", "Pages"=>tree});
        pdf.trailer.set("Root", catalog);
        pdf.save(bundle.join(format!("{ID}.pdf"))).unwrap();
        fs::write(
            bundle.join(format!("{ID}.content")),
            json!({"formatVersion":1,"fileType":"pdf",
                   "pages":["00000000-0000-4000-8000-000000000001"],
                   "redirectionPageMap":[0],"originalPageCount":1})
            .to_string(),
        )
        .unwrap();
        fs::write(
            bundle.join(format!("{ID}.metadata")),
            json!({"visibleName":"Long plan","type":"DocumentType","parent":""}).to_string(),
        )
        .unwrap();

        let output = tempfile::tempdir().unwrap();
        let out = output.path().join("review");
        let report = render_review(directory, ID, 800, &out, FetchContext::default()).unwrap();
        let page = &report["pages"][0];
        // As wide as a 3:4 page of 800 pixels, and as tall as the page needs.
        assert_eq!(page["size"], json!([600, 4000]), "{page}");
        let parts = page["parts"].as_array().unwrap();
        let rows: Vec<(u64, u64)> = parts
            .iter()
            .map(|part| {
                (
                    part["top"].as_u64().unwrap(),
                    part["height"].as_u64().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            rows,
            [
                (0, 800),
                (700, 800),
                (1400, 800),
                (2100, 800),
                (2800, 800),
                (3200, 800)
            ]
        );
        let blue = |name: &str| {
            let image = image::open(out.join(name)).unwrap().to_rgb8();
            assert_eq!(image.dimensions(), (600, 800));
            image
                .pixels()
                .filter(|p| p[0] < 60 && p[1] < 60 && p[2] > 200)
                .count()
        };
        // The bar is drawn sharply in the last part and nowhere near the top.
        assert!(blue("page-001-part-06.png") > 5000);
        assert_eq!(blue("page-001-part-01.png"), 0);
    }

    #[test]
    fn long_page_parts_overlap_and_cover_the_page() {
        assert!(long_page_parts(600, 800).is_empty());
        assert!(long_page_parts(600, 830).is_empty());
        assert_eq!(long_page_parts(600, 1000), [(0, 800), (200, 800)]);
        assert_eq!(
            clip_rows([10, 650, 50, 300], 700, 800),
            Some([10, 0, 50, 250])
        );
        assert_eq!(clip_rows([10, 100, 50, 300], 700, 800), None);
    }

    /// Render a real pulled bundle the way `turborm review fetch` does:
    /// `TURBORM_REVIEW_BUNDLE=<pull directory> TURBORM_REVIEW_OUT=<new dir>`,
    /// optionally `TURBORM_REVIEW_PAGES=<first N pages>` (format-1 content)
    /// and `TURBORM_REVIEW_SIZE=<longest side>`. Skipped without a bundle.
    #[test]
    fn renders_a_review_of_a_bundle_when_one_is_supplied() {
        let Ok(source) = std::env::var("TURBORM_REVIEW_BUNDLE") else {
            return;
        };
        let out = PathBuf::from(std::env::var("TURBORM_REVIEW_OUT").expect("TURBORM_REVIEW_OUT"));
        let source = Path::new(&source);
        let id = fs::read_dir(source)
            .unwrap()
            .flatten()
            .find_map(|entry| {
                let name = entry.file_name().into_string().ok()?;
                name.strip_suffix(".content").map(str::to_owned)
            })
            .expect("a .content file");
        let directory = tempfile::tempdir().unwrap();
        let bundle = directory.path().join("bundle");
        let copy = |from: &Path, to: &Path| {
            let status = std::process::Command::new("cp")
                .arg("-r")
                .arg(from)
                .arg(to)
                .status()
                .unwrap();
            assert!(status.success());
        };
        copy(source, &bundle);
        if let Some(limit) = std::env::var("TURBORM_REVIEW_PAGES")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
        {
            let path = bundle.join(format!("{id}.content"));
            let mut content: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            for key in ["pages", "redirectionPageMap"] {
                if let Some(list) = content[key].as_array_mut() {
                    list.truncate(limit);
                }
            }
            fs::write(&path, content.to_string()).unwrap();
        }
        let size = std::env::var("TURBORM_REVIEW_SIZE")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(DEFAULT_LONGEST_SIDE);
        let report = render_review(directory, &id, size, &out, FetchContext::default()).unwrap();
        println!("{}", fs::read_to_string(out.join("summary.md")).unwrap());
        assert!(!report["pages"].as_array().unwrap().is_empty());
    }
}
