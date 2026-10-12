//! Read-only cloud library snapshots and verified local downloads.
use crate::{
    cloud::{self, Transport, JSON_LIMIT},
    hash_tree, job,
    manifest::{Manifest, Row},
};
use anyhow::{anyhow, bail, ensure, Context, Result};
use serde::Serialize;
use serde_json::Value;
use std::{
    collections::{HashMap, HashSet},
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Mutex,
    },
};
use uuid::Uuid;

pub const DEFAULT_MAX_BYTES: usize = 512 * 1024 * 1024;

#[derive(Clone, Debug, Serialize)]
pub struct Entry {
    pub id: String,
    pub title: String,
    pub kind: String,
    pub parent: String,
    pub deleted: bool,
    pub pinned: bool,
    pub modified: Option<String>,
    pub files: u32,
    pub size: u64,
    pub path: String,
    pub hierarchy_warning: Option<String>,
    /// "notebook", "pdf" or "epub" when the document's `.content` blob is
    /// readable. Folders and unreadable descriptors leave this `None`.
    pub file_type: Option<String>,
    /// Page count from `.content` (`pageCount`, else the length of
    /// `cPages.pages`). `None` when absent or not a document.
    pub page_count: Option<u32>,
}
#[derive(Serialize)]
pub struct Listing {
    pub generation: u64,
    pub entries: Vec<Entry>,
    pub failures: Vec<ListingFailure>,
}
#[derive(Clone, Debug, Serialize)]
pub struct ListingFailure {
    pub id: String,
    pub error: String,
}
#[derive(Serialize)]
pub struct FileInfo {
    pub path: String,
    pub bytes: u64,
    pub sha256: String,
}
#[derive(Serialize)]
pub struct DocumentInfo {
    pub entry: Entry,
    pub generation: u64,
    pub metadata: Value,
    pub content: Option<Value>,
    pub files: Vec<FileInfo>,
}
pub struct Snapshot {
    pub generation: u64,
    pub manifest: Manifest,
}
pub struct Library<'a, T: Transport> {
    http: cloud::ReadOnly<'a, T>,
    host: &'a str,
    token: &'a str,
    cache: Option<PathBuf>,
}
pub fn default_cache_dir() -> Result<PathBuf> {
    #[cfg(windows)]
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("LOCALAPPDATA unavailable; supply --cache-dir"))?;
    #[cfg(not(windows))]
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .ok_or_else(|| anyhow!("Cache directory unavailable; supply --cache-dir"))?;
    Ok(base.join("remarkable-send").join("cache"))
}
impl<'a, T: Transport> Library<'a, T> {
    pub fn new(http: &'a T, host: &'a str, token: &'a str, cache: Option<PathBuf>) -> Self {
        Self {
            http: cloud::ReadOnly::new(http),
            host,
            token,
            cache,
        }
    }
    pub fn snapshot(&self) -> Result<Snapshot> {
        let root = hash_tree::read_root(&self.http, self.host, self.token)?;
        let manifest = hash_tree::get_manifest(&self.http, self.host, self.token, &root.hash, ".")?;
        for row in &manifest.rows {
            canonical_id(&row.name)?;
        }
        Ok(Snapshot {
            generation: root.generation,
            manifest,
        })
    }
    fn blob(&self, row: &Row, limit: usize, cache: bool) -> Result<Vec<u8>> {
        ensure!(
            row.size <= limit as u64,
            "Cloud file exceeds download limit; raise --max-bytes for a selected download"
        );
        let bytes = self.cached(&row.hash, &row.name, limit, cache, |b| {
            ensure!(
                job::hash(b) == row.hash && b.len() as u64 == row.size,
                "Cloud file checksum or length mismatch"
            );
            Ok(())
        })?;
        Ok(bytes)
    }
    fn cached(
        &self,
        hash: &str,
        filename: &str,
        limit: usize,
        use_cache: bool,
        validate: impl Fn(&[u8]) -> Result<()>,
    ) -> Result<Vec<u8>> {
        ensure!(cloud::is_hash(hash), "Invalid cloud address");
        let path = self
            .cache
            .as_ref()
            .filter(|_| use_cache)
            .map(|p| p.join(hash));
        if let Some(path) = &path {
            if let Ok(bytes) = job::read_bounded(path, limit) {
                if validate(&bytes).is_ok() {
                    return Ok(bytes);
                }
            }
        }
        let bytes = self.http.request(
            self.host,
            "GET",
            &format!("/sync/v3/files/{hash}"),
            Some(self.token),
            None,
            &[("rm-filename", filename.into())],
            limit,
        )?;
        validate(&bytes)?;
        if let Some(path) = path {
            let parent = path.parent().unwrap();
            private_dir(parent)?;
            let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
            tmp.write_all(&bytes)?;
            tmp.persist(&path)
                .map_err(|_| anyhow!("Cannot save verified metadata cache"))?;
        }
        Ok(bytes)
    }
    pub fn document(&self, row: &Row) -> Result<Manifest> {
        canonical_id(&row.name)?;
        let bytes = self.cached(
            &row.hash,
            &format!("{}.docSchema", row.name),
            JSON_LIMIT,
            true,
            |b| parse_document(b, row).map(|_| ()),
        )?;
        parse_document(&bytes, row)
    }
    /// Read one verified component from an immutable snapshot. Thumbnail
    /// generation uses this instead of downloading every notebook page.
    pub fn component(
        &self,
        snapshot: &Snapshot,
        id: &str,
        path: &str,
        max_bytes: usize,
    ) -> Result<Vec<u8>> {
        let document = self.document(find_row(snapshot, id)?)?;
        let row = document
            .rows
            .iter()
            .find(|row| row.name == path)
            .ok_or_else(|| anyhow!("Selected document component is absent"))?;
        self.blob(row, max_bytes, false)
    }
    /// [`Library::component`] through a size-bounded on-disk cache, for the
    /// viewer: reopening a document reuses its downloaded PDF and pages.
    ///
    /// Components are stored by content hash under `components/` in the cache
    /// directory and verified again on every read. After each store the least
    /// recently used files are removed until the directory fits `budget`
    /// bytes; a component larger than half the budget is never stored.
    pub fn cached_component(
        &self,
        snapshot: &Snapshot,
        id: &str,
        path: &str,
        max_bytes: usize,
        budget: u64,
    ) -> Result<Vec<u8>> {
        let document = self.document(find_row(snapshot, id)?)?;
        let row = document
            .rows
            .iter()
            .find(|row| row.name == path)
            .ok_or_else(|| anyhow!("Selected document component is absent"))?;
        ensure!(cloud::is_hash(&row.hash), "Invalid cloud address");
        let Some(directory) = self.cache.as_deref().map(component_dir) else {
            return self.blob(row, max_bytes, false);
        };
        let stored = directory.join(&row.hash);
        if row.size <= max_bytes as u64 {
            if let Ok(bytes) = job::read_bounded(&stored, max_bytes) {
                if bytes.len() as u64 == row.size && job::hash(&bytes) == row.hash {
                    // Mark it recently used; eviction goes by modification time.
                    if let Ok(file) = fs::File::options().append(true).open(&stored) {
                        let _ = file.set_modified(std::time::SystemTime::now());
                    }
                    return Ok(bytes);
                }
            }
        }
        let bytes = self.blob(row, max_bytes, false)?;
        if (bytes.len() as u64) <= budget / 2 {
            // A cache that cannot be written only costs a later download.
            let _ = store_component(&directory, &stored, &bytes, budget);
        }
        Ok(bytes)
    }
    /// The component rows of one document in `snapshot`, read through the
    /// verified document manifest. Nothing but that small manifest is read.
    pub fn document_rows(&self, snapshot: &Snapshot, id: &str) -> Result<Vec<Row>> {
        Ok(self.document(find_row(snapshot, id)?)?.rows)
    }
    fn metadata(&self, row: &Row) -> Result<(Manifest, Value)> {
        let doc = self.document(row)?;
        let name = format!("{}.metadata", row.name);
        let file = doc
            .rows
            .iter()
            .find(|r| r.name == name)
            .ok_or_else(|| anyhow!("Document has no metadata file"))?;
        let value = cloud::parse_json(&self.blob(file, JSON_LIMIT, true)?)?;
        ensure!(value.is_object(), "Metadata must be a JSON object");
        Ok((doc, value))
    }
    /// Reads the small `.content` descriptor through the same bounded, verified
    /// and hash-cached path as `.metadata`. A document without a readable
    /// `.content` is not a listing failure: the caller keeps the entry and
    /// leaves the file type unknown.
    fn content(&self, doc: &Manifest, id: &str) -> Option<Value> {
        let row = doc
            .rows
            .iter()
            .find(|r| r.name == format!("{id}.content"))?;
        self.blob(row, JSON_LIMIT, true)
            .ok()
            .and_then(|bytes| cloud::parse_json(&bytes).ok())
            .filter(Value::is_object)
    }
    fn entry(&self, row: &Row) -> Result<Entry> {
        let (doc, meta) = self.metadata(row)?;
        let mut entry = entry_from_metadata(row, &meta)?;
        if entry.kind == "document" {
            if let Some(content) = self.content(&doc, &row.name) {
                apply_content(&mut entry, &content);
            }
        }
        Ok(entry)
    }
    pub fn info(&self, snapshot: &Snapshot, id: &str) -> Result<DocumentInfo> {
        let row = find_row(snapshot, id)?;
        let (doc, metadata) = self.metadata(row)?;
        let mut first = entry_from_metadata(row, &metadata)?;
        if first.kind == "document" {
            if let Some(content) = self.content(&doc, &row.name) {
                apply_content(&mut first, &content);
            }
        }
        let mut ancestry = vec![first];
        let mut parent = ancestry[0].parent.clone();
        let mut seen = HashSet::from([id.to_owned()]);
        while !parent.is_empty() && parent != "trash" && seen.insert(parent.clone()) {
            let Some(parent_row) = snapshot.manifest.rows.iter().find(|r| r.name == parent) else {
                break;
            };
            let ancestor = self.entry(parent_row)?;
            parent = ancestor.parent.clone();
            ancestry.push(ancestor);
        }
        resolve_paths(&mut ancestry);
        let entry = ancestry.remove(0);
        let content = doc
            .rows
            .iter()
            .find(|r| r.name == format!("{id}.content"))
            .map(|r| {
                self.blob(r, JSON_LIMIT, true)
                    .and_then(|b| cloud::parse_json(&b))
            })
            .transpose()?;
        Ok(DocumentInfo {
            entry,
            generation: snapshot.generation,
            metadata,
            content,
            files: doc
                .rows
                .iter()
                .map(|r| FileInfo {
                    path: r.name.clone(),
                    bytes: r.size,
                    sha256: r.hash.clone(),
                })
                .collect(),
        })
    }
    /// Downloads exactly the selected immutable snapshot into a NEW directory.
    pub fn pull(
        &self,
        snapshot: &Snapshot,
        id: &str,
        out: &Path,
        max_bytes: usize,
    ) -> Result<usize> {
        ensure!(
            !out.exists(),
            "Output already exists; choose a new bundle directory"
        );
        let row = find_row(snapshot, id)?;
        let doc = self.document(row)?;
        let paths = validate_paths(&doc, id)?;
        ensure!(
            doc.rows.iter().all(|r| r.size <= max_bytes as u64),
            "A component exceeds --max-bytes"
        );
        let parent = output_parent(out)?;
        let staging = tempfile::Builder::new()
            .prefix(".remarkable-pull-")
            .tempdir_in(parent)?;
        for (file, path) in doc.rows.iter().zip(paths) {
            let bytes = self.blob(file, max_bytes, false)?;
            let target = staging.path().join(path);
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut output = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(target)?;
            output.write_all(&bytes)?;
            output.sync_all()?;
        }
        // Keep original cloud schema bytes, including a legacy schema if encountered.
        let raw = self.cached(
            &row.hash,
            &format!("{id}.docSchema"),
            JSON_LIMIT,
            true,
            |b| parse_document(b, row).map(|_| ()),
        )?;
        fs::write(staging.path().join(format!("{id}.docSchema")), raw)?;
        fs::write(
            staging.path().join("download.json"),
            serde_json::to_vec_pretty(&serde_json::json!({
                "version":1,"id":id,"generation":snapshot.generation,"document_hash":row.hash,
                "files":doc.rows.iter().map(|r|serde_json::json!({"path":r.name,"size":r.size,"sha256":r.hash})).collect::<Vec<_>>()
            }))?,
        )?;
        // Reserve destination atomically; never merge with an existing directory.
        fs::create_dir(out).context("Cannot reserve a new output directory")?;
        // A failure here retains verified staging data, with its location in the error.
        let retained = staging.keep();
        let mut children = fs::read_dir(&retained)?.collect::<std::io::Result<Vec<_>>>()?;
        // The completion inventory is always moved last, after every native file.
        children.sort_by_key(|child| child.file_name() == "download.json");
        for child in children {
            fs::rename(child.path(), out.join(child.file_name())).with_context(|| {
                format!(
                    "Local finalization interrupted; verified files remain in {}",
                    retained.display()
                )
            })?;
        }
        fs::remove_dir(retained)?;
        Ok(doc.rows.len())
    }
    /// Exports the original PDF/EPUB, never renders handwritten overlays.
    pub fn export(
        &self,
        snapshot: &Snapshot,
        id: &str,
        out: &Path,
        max_bytes: usize,
    ) -> Result<&'static str> {
        ensure!(
            !out.exists(),
            "Output already exists; export will not overwrite it"
        );
        let doc = self.document(find_row(snapshot, id)?)?;
        let content = doc
            .rows
            .iter()
            .find(|r| r.name == format!("{id}.content"))
            .ok_or_else(|| anyhow!("No content descriptor; use pull for native files"))?;
        let value = cloud::parse_json(&self.blob(content, JSON_LIMIT, true)?)?;
        let ext=match value["fileType"].as_str() { Some("pdf")=>"pdf",Some("epub")=>"epub",_=>bail!("This notebook has no original PDF/EPUB export. Use pull for its native files; handwriting rendering is not implemented") };
        ensure!(
            out.extension()
                .and_then(|v| v.to_str())
                .is_some_and(|v| v.eq_ignore_ascii_case(ext)),
            "Output extension must match the document's original .{ext} format"
        );
        let file = doc
            .rows
            .iter()
            .find(|r| r.name == format!("{id}.{ext}"))
            .ok_or_else(|| {
                anyhow!("Original source file is absent; use pull for available native files")
            })?;
        let bytes = self.blob(file, max_bytes, false)?;
        let mut tmp = tempfile::NamedTempFile::new_in(output_parent(out)?)?;
        tmp.write_all(&bytes)?;
        tmp.as_file().sync_all()?;
        tmp.persist_noclobber(out)
            .map_err(|_| anyhow!("Cannot create export without overwriting an existing file"))?;
        Ok(ext)
    }
}
impl<T: Transport + Sync> Library<'_, T> {
    pub fn list(&self, snapshot: &Snapshot, jobs: usize) -> Result<Listing> {
        ensure!((1..=8).contains(&jobs), "--jobs must be between 1 and 8");
        let next = AtomicUsize::new(0);
        let completed = AtomicUsize::new(0);
        let entries = Mutex::new(Vec::new());
        let errors = Mutex::new(Vec::new());
        std::thread::scope(|scope| {
            for _ in 0..jobs {
                let (next, completed, entries, errors) = (&next, &completed, &entries, &errors);
                scope.spawn(move || loop {
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    let Some(row) = snapshot.manifest.rows.get(index) else {
                        break;
                    };
                    match self.entry(row) {
                        Ok(e) => entries.lock().unwrap().push(e),
                        Err(error) => errors.lock().unwrap().push(ListingFailure {
                            id: row.name.clone(),
                            error: format!("{error:#}"),
                        }),
                    }
                    let count = completed.fetch_add(1, Ordering::Relaxed) + 1;
                    if count % 100 == 0 {
                        eprintln!(
                            "Read {count}/{} library entries...",
                            snapshot.manifest.rows.len()
                        );
                    }
                });
            }
        });
        let total = snapshot.manifest.rows.len();
        eprintln!("Read {total}/{total} library entries.");
        let mut entries = entries.into_inner().unwrap();
        let mut failures = errors.into_inner().unwrap();
        resolve_paths(&mut entries);
        entries.sort_by(|a, b| a.path.cmp(&b.path).then(a.id.cmp(&b.id)));
        failures.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(Listing {
            generation: snapshot.generation,
            entries,
            failures,
        })
    }
}
fn canonical_id(id: &str) -> Result<()> {
    ensure!(
        Uuid::parse_str(id).is_ok_and(|u| !u.is_nil() && u.to_string() == id),
        "Use a canonical document/folder UUID from list"
    );
    Ok(())
}
fn find_row<'a>(snapshot: &'a Snapshot, id: &str) -> Result<&'a Row> {
    canonical_id(id)?;
    snapshot
        .manifest
        .rows
        .iter()
        .find(|r| r.name == id)
        .ok_or_else(|| anyhow!("Document/folder ID is not present in this cloud snapshot"))
}
fn parse_document(bytes: &[u8], row: &Row) -> Result<Manifest> {
    // Schema-4 identity includes every byte. Older schemas fail closed.
    let doc = if bytes.starts_with(b"4\n") {
        ensure!(
            job::hash(bytes) == row.hash,
            "Document manifest checksum mismatch"
        );
        Manifest::parse(bytes)?
    } else {
        bail!("Unsupported document schema; expected schema 4")
    };
    ensure!(
        doc.name == row.name
            && doc.rows.len() == row.count as usize
            && doc.total_size()? == row.size,
        "Document descriptor differs from root snapshot"
    );
    validate_paths(&doc, &row.name)?;
    Ok(doc)
}
fn entry_from_metadata(row: &Row, meta: &Value) -> Result<Entry> {
    let title = meta["visibleName"]
        .as_str()
        .ok_or_else(|| anyhow!("Metadata title missing"))?
        .to_owned();
    let parent = meta["parent"]
        .as_str()
        .ok_or_else(|| anyhow!("Metadata parent missing"))?
        .to_owned();
    let kind = match meta["type"].as_str() {
        Some("CollectionType") => "folder",
        Some("DocumentType") => "document",
        _ => "unknown",
    }
    .to_owned();
    Ok(Entry {
        id: row.name.clone(),
        title,
        parent: parent.clone(),
        kind,
        deleted: meta["deleted"].as_bool().unwrap_or(false) || parent == "trash",
        pinned: meta["pinned"].as_bool().unwrap_or(false),
        modified: meta["lastModified"].as_str().map(str::to_owned),
        files: row.count,
        size: row.size,
        path: String::new(),
        hierarchy_warning: None,
        file_type: None,
        page_count: None,
    })
}
/// Applies the `.content` descriptor to an entry. Only the recognized document
/// file types are stored, so an unexpected value reads as unknown rather than
/// reaching the interface as an arbitrary string.
pub fn apply_content(entry: &mut Entry, content: &Value) {
    entry.file_type = match content["fileType"].as_str() {
        Some("notebook") => Some("notebook".to_owned()),
        Some("pdf") => Some("pdf".to_owned()),
        Some("epub") => Some("epub".to_owned()),
        _ => None,
    };
    entry.page_count = content["pageCount"]
        .as_u64()
        .or_else(|| {
            content["cPages"]["pages"]
                .as_array()
                .map(|p| p.len() as u64)
        })
        .and_then(|value| u32::try_from(value).ok());
}
pub fn resolve_paths(entries: &mut [Entry]) {
    let map: HashMap<_, _> = entries
        .iter()
        .map(|e| (e.id.clone(), (e.title.clone(), e.parent.clone(), e.deleted)))
        .collect();
    for entry in entries {
        let mut parts = vec![entry.title.clone()];
        let mut parent = entry.parent.clone();
        let mut visited = HashSet::from([entry.id.clone()]);
        while !parent.is_empty() {
            if parent == "trash" {
                entry.deleted = true;
                parts.push("[Trash]".into());
                break;
            }
            if !visited.insert(parent.clone()) {
                entry.hierarchy_warning = Some("parent cycle".into());
                parts.push("[Cycle]".into());
                break;
            }
            if let Some((title, next, deleted)) = map.get(&parent) {
                parts.push(title.clone());
                entry.deleted |= *deleted;
                parent = next.clone();
            } else {
                entry.hierarchy_warning = Some("parent absent from snapshot".into());
                parts.push("[Missing parent]".into());
                break;
            }
        }
        parts.reverse();
        entry.path = format!("/{}", parts.join("/"));
    }
}
pub fn filter_listing(
    listing: &mut Listing,
    folder: Option<&str>,
    recursive: bool,
    include_trash: bool,
) -> Result<()> {
    if let Some(id) = folder {
        if id != "root" {
            canonical_id(id)?;
            ensure!(
                listing
                    .entries
                    .iter()
                    .any(|e| e.id == id && e.kind == "folder"),
                "Folder ID is not a folder in this snapshot"
            );
        }
    }
    let map: HashMap<_, _> = listing
        .entries
        .iter()
        .map(|e| (e.id.clone(), e.parent.clone()))
        .collect();
    listing.entries.retain(|e| {
        if e.deleted && !include_trash {
            return false;
        }
        let Some(folder) = folder else {
            return true;
        };
        let target = if folder == "root" { "" } else { folder };
        let mut parent = e.parent.as_str();
        let mut visited = HashSet::new();
        loop {
            if parent == target {
                return true;
            }
            if !recursive || parent.is_empty() || !visited.insert(parent) {
                return false;
            }
            match map.get(parent) {
                Some(next) => parent = next,
                None => return false,
            }
        }
    });
    Ok(())
}
/// Portable extraction names: reject traversal, drive/ADS names and Windows aliases.
fn validate_paths(doc: &Manifest, id: &str) -> Result<Vec<PathBuf>> {
    let mut seen = HashSet::new();
    let mut result = Vec::new();
    for row in &doc.rows {
        ensure!(
            row.count == 0,
            "Nested manifest rows are unsupported in a document"
        );
        ensure!(
            row.name != format!("{id}.docSchema"),
            "Component collides with reserved bundle manifest"
        );
        ensure!(
            row.name.starts_with(&format!("{id}.")) || row.name.starts_with(&format!("{id}/")),
            "Component belongs to another document"
        );
        ensure!(
            !row.name.contains('\\'),
            "Backslash in remote component path"
        );
        for component in row.name.split('/') {
            ensure!(
                !component.is_empty()
                    && component != "."
                    && component != ".."
                    && !component.ends_with(['.', ' '])
                    && !component
                        .chars()
                        .any(|c| c.is_control() || "<>:\"|?*".contains(c)),
                "Unsafe remote component path"
            );
            let stem = component.split('.').next().unwrap().to_ascii_uppercase();
            ensure!(
                !matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
                    && !(stem.len() == 4
                        && (stem.starts_with("COM") || stem.starts_with("LPT"))
                        && matches!(stem.as_bytes()[3], b'1'..=b'9')),
                "Reserved remote component path"
            );
        }
        let lower = row.name.to_lowercase();
        ensure!(seen.insert(lower), "Case-colliding remote paths");
        result.push(PathBuf::from(&row.name));
    }
    for name in &seen {
        for (index, _) in name.match_indices('/') {
            ensure!(
                !seen.contains(&name[..index]),
                "File/directory path collision"
            );
        }
    }
    Ok(result)
}
fn output_parent(out: &Path) -> Result<&Path> {
    let parent = out
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    ensure!(parent.is_dir(), "Output parent directory does not exist");
    Ok(parent)
}
/// Save one verified component, then trim the directory to `budget` bytes by
/// removing the least recently used files first.
/// Where [`Library::cached_component`] keeps components under `cache`.
fn component_dir(cache: &Path) -> PathBuf {
    cache.join("components")
}

/// Whether the component with this content `hash` and `size` is already in
/// the component cache under `cache`, judged by name and length only:
/// nothing is read. Every read verifies it again.
pub fn is_component_cached(cache: &Path, hash: &str, size: u64) -> bool {
    cloud::is_hash(hash)
        && fs::metadata(component_dir(cache).join(hash))
            .is_ok_and(|metadata| metadata.is_file() && metadata.len() == size)
}

/// Bytes the component cache under `cache` takes on disk now.
pub fn component_cache_bytes(cache: &Path) -> u64 {
    fs::read_dir(component_dir(cache))
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.ok()?.metadata().ok())
        .filter(|metadata| metadata.is_file())
        .map(|metadata| metadata.len())
        .sum()
}

/// Remove the least recently used components until the cache under `cache`
/// fits `budget` bytes.
pub fn trim_component_cache(cache: &Path, budget: u64) -> Result<()> {
    evict(&component_dir(cache), budget, None)
}

/// Remove every cached component whose hash is not in `keep`, and return the
/// bytes freed. An empty `keep` clears the component cache.
pub fn prune_component_cache(cache: &Path, keep: &HashSet<String>) -> Result<u64> {
    let directory = component_dir(cache);
    let Ok(entries) = fs::read_dir(&directory) else {
        return Ok(0);
    };
    let mut freed = 0;
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        // Only finished components; a download being saved is left alone.
        let Some(name) = name.to_str().filter(|name| cloud::is_hash(name)) else {
            continue;
        };
        if keep.contains(name) {
            continue;
        }
        let size = entry.metadata().map_or(0, |metadata| metadata.len());
        if fs::remove_file(entry.path()).is_ok() {
            freed += size;
        }
    }
    Ok(freed)
}

fn store_component(directory: &Path, stored: &Path, bytes: &[u8], budget: u64) -> Result<()> {
    private_dir(directory)?;
    let mut tmp = tempfile::NamedTempFile::new_in(directory)?;
    tmp.write_all(bytes)?;
    tmp.persist(stored)
        .map_err(|_| anyhow!("Cannot save the component cache"))?;
    // An unbounded cache never evicts, so it skips the directory scan.
    if budget == u64::MAX {
        return Ok(());
    }
    evict(directory, budget, Some(stored))
}

/// Remove the oldest files in `directory` until it fits `budget`, never
/// `keep`. Eviction goes by modification time, which a cache hit refreshes.
fn evict(directory: &Path, budget: u64, keep: Option<&Path>) -> Result<()> {
    let Ok(entries) = fs::read_dir(directory) else {
        return Ok(());
    };
    let mut files = Vec::new();
    for entry in entries {
        let entry = entry?;
        let metadata = entry.metadata()?;
        if metadata.is_file() {
            files.push((metadata.modified()?, metadata.len(), entry.path()));
        }
    }
    let mut total: u64 = files.iter().map(|(_, size, _)| size).sum();
    if total <= budget {
        return Ok(());
    }
    files.sort();
    for (_, size, path) in files {
        if total <= budget {
            break;
        }
        if Some(path.as_path()) != keep && fs::remove_file(&path).is_ok() {
            total -= size;
        }
    }
    Ok(())
}

fn private_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

#[cfg(test)]
#[path = "library_tests.rs"]
mod tests;
