//! Application operations, independent of widgets and drawing.
use crate::renderer::{Document, Preview, Tile, TileKey};
use crate::settings::CacheMode;
use anyhow::{anyhow, ensure, Context, Result};
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
use turborm_core::{
    auth, folders, hash_tree, import, job,
    library::{self, Entry, Library},
    moves,
};
use zeroize::Zeroizing;

/// The most tile workers to start. Tiles are CPU-bound rasterizing, so more
/// threads than physical cores only adds contention, and four is already far
/// past the point where a viewport's worth of tiles lands in one frame.
pub const MAX_TILE_WORKERS: usize = 4;

/// Free disk the full-library download always leaves, so saving the library
/// offline can never fill the disk.
pub const OFFLINE_FREE_SPACE_RESERVE: u64 = 2 * 1024 * 1024 * 1024;

/// Download limits for the document files the viewer reads.
const PDF_LIMIT: usize = 512 * 1024 * 1024;
const PAGEDATA_LIMIT: usize = 4 * 1024 * 1024;
const SCENE_LIMIT: usize = 64 * 1024 * 1024;

pub enum Command {
    Connect,
    Pair(Zeroizing<String>),
    Refresh,
    Open {
        id: String,
    },
    Page(usize),
    /// Every visible tile, ordered centre-out by the caller.
    Tiles(Vec<TileKey>),
    Thumbnail {
        id: String,
        epoch: u64,
    },
    Import {
        parent: String,
    },
    Move {
        id: String,
        parent: String,
    },
    /// Add one new folder under `parent` ("" for the library root).
    CreateFolder {
        parent: String,
        name: String,
    },
    ExportOriginal {
        id: String,
    },
    ExportNative {
        id: String,
    },
    /// The open document as a preview PDF.
    ExportPdf,
    /// One page of the open document as SVG.
    ExportSvg(usize),
    /// Every page of the open document as SVG.
    ExportSvgPages,
    /// A document from the library, without opening it in the viewer.
    ExportDocument {
        id: String,
        format: ExportFormat,
    },
    VerifyImport,
    /// Keep more or less of the library on disk. Takes effect at once: a
    /// smaller cache is trimmed, and "full" starts the background download.
    SetCacheMode(CacheMode),
    /// Report the disk used by downloaded documents and the library's size.
    MeasureStorage,
    /// Remove every downloaded document from the disk cache.
    ClearCache,
}

/// What an export from the library writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExportFormat {
    /// The rasterized preview PDF the viewer can also export.
    PreviewPdf,
    /// One SVG for a single page; a folder of page1.svg, page2.svg… otherwise.
    Svg,
}

/// Progress of the background download that keeps the library offline.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Offline {
    /// Not in "full" mode, or not started yet.
    Idle,
    /// Downloading: `done` of the `total` bytes missing when the pass began.
    Saving { done: u64, total: u64 },
    /// Every document is on disk; the library takes `library` bytes.
    Ready { library: u64 },
    /// The pass stopped early; the reason is for the user.
    Stopped(String),
}

pub enum Event {
    Paired(bool),
    Listing(Vec<Entry>),
    /// Sent before any download, so the viewer can switch screens at once with
    /// the title, the page count, the page it opens on and whatever
    /// placeholder it already has.
    Opening {
        title: String,
        pages: usize,
        page: usize,
    },
    Preview(Preview),
    Tile(Result<Tile, String>),
    Thumbnail {
        id: String,
        epoch: u64,
        preview: Result<Preview, String>,
    },
    Message(String),
    Error(String),
    /// A folder was created and the listing refreshed; select it.
    FolderCreated(String),
    /// Bytes the downloaded-document cache takes on disk now.
    Storage(u64),
    /// Bytes every document in the library would take on disk.
    LibrarySize(u64),
    Offline(Offline),
    Finished,
}

/// The cache mode every worker reads. The settings dialog changes it while
/// the app runs, and each download picks up the new budget.
#[derive(Clone)]
struct Policy(Arc<Mutex<CacheMode>>);

impl Policy {
    fn mode(&self) -> CacheMode {
        self.0.lock().map_or(CacheMode::default(), |mode| *mode)
    }
    fn set(&self, mode: CacheMode) {
        if let Ok(mut slot) = self.0.lock() {
            *slot = mode;
        }
    }
    fn budget(&self) -> u64 {
        self.mode().budget()
    }
}

/// Everything [`handle`] hands work to besides the foreground worker.
struct Workers {
    thumb_tx: mpsc::SyncSender<(String, u64)>,
    tile_tx: Sender<TileKey>,
    source: TileSource,
    /// Wakes the full-library worker; each message asks for one more pass.
    offline_tx: Sender<()>,
    policy: Policy,
    /// Set while the foreground worker runs a command, so the full-library
    /// download stands aside for whatever the user asked for.
    busy: Arc<AtomicBool>,
}

/// The rendered document the tile workers share with the foreground worker.
///
/// Tile rendering never blocks on a refresh, a listing or an export, and a
/// foreground page render never waits for a tile. The `Document` itself owns
/// the page cache behind its own lock.
#[derive(Clone, Default)]
struct TileSource(Arc<Mutex<Option<Arc<Document>>>>);

impl TileSource {
    fn set(&self, document: Option<Arc<Document>>) {
        if let Ok(mut slot) = self.0.lock() {
            *slot = document;
        }
    }
    fn get(&self) -> Option<Arc<Document>> {
        self.0.lock().ok()?.clone()
    }
}

/// One worker serializes foreground operations; a bounded worker loads covers.
/// Widgets only exchange typed commands/results, without credentials or HTTP.
pub fn start(
    mode: CacheMode,
    wake: impl Fn() + Send + Sync + 'static,
) -> (Sender<Command>, Receiver<Event>) {
    let (commands, receiver) = mpsc::channel();
    let (sender, events) = mpsc::channel();
    let wake = Arc::new(wake);
    let policy = Policy(Arc::new(Mutex::new(mode)));
    let busy = Arc::new(AtomicBool::new(false));
    // The full-library download has its own worker: it is slow, it only
    // fills the disk cache, and it pauses whenever the foreground is busy.
    let (offline_tx, offline_rx) = mpsc::channel::<()>();
    {
        let policy = policy.clone();
        let busy = busy.clone();
        let sender = sender.clone();
        let wake = wake.clone();
        std::thread::spawn(move || {
            while offline_rx.recv().is_ok() {
                // Several refreshes in a row need only one pass.
                while offline_rx.try_recv().is_ok() {}
                if policy.mode() != CacheMode::Full {
                    continue;
                }
                let mut report = |progress: Offline| {
                    let _ = sender.send(Event::Offline(progress));
                    wake();
                };
                let outcome = save_library_offline(&policy, &busy, &mut report)
                    .unwrap_or_else(|error| Offline::Stopped(format!("{error:#}")));
                if let Offline::Ready { library } = outcome {
                    let _ = sender.send(Event::LibrarySize(library));
                }
                // A pass cut short by a mode change has nothing to report.
                if policy.mode() == CacheMode::Full {
                    report(outcome);
                }
            }
        });
    }
    // Thumbnails have their own bounded worker so a slow cloud cover never
    // holds up opening a document or rendering the visible canvas.
    let (thumb_tx, thumb_rx) = mpsc::sync_channel::<(String, u64)>(1);
    let thumb_sender = sender.clone();
    let thumb_wake = wake.clone();
    let thumb_policy = policy.clone();
    std::thread::spawn(move || {
        let mut context: Option<(u64, Session, library::Snapshot)> = None;
        while let Ok((id, epoch)) = thumb_rx.recv() {
            if context
                .as_ref()
                .is_none_or(|(cached_epoch, _, _)| *cached_epoch != epoch)
            {
                context = match thumbnail_context() {
                    Ok((session, snapshot)) => Some((epoch, session, snapshot)),
                    Err(error) => {
                        let _ = thumb_sender.send(Event::Thumbnail {
                            id,
                            epoch,
                            preview: Err(format!("{error:#}")),
                        });
                        thumb_wake();
                        continue;
                    }
                };
            }
            let preview = match context.as_ref() {
                Some((_, session, snapshot)) => {
                    thumbnail(session, snapshot, &id, thumb_policy.budget())
                        .map_err(|error| format!("{error:#}"))
                }
                None => Err("Thumbnail session is unavailable; refresh to retry".into()),
            };
            if preview.is_err() {
                // The session or immutable snapshot may have gone stale. Rebuild
                // both before the next cover request in this epoch.
                context = None;
            }
            if thumb_sender
                .send(Event::Thumbnail { id, epoch, preview })
                .is_err()
            {
                break;
            }
            thumb_wake();
        }
    });
    // Tiles render on their own workers, sharing the rendered page through an
    // Arc. A refresh, a listing or an export on the foreground worker can no
    // longer stall the visible canvas, and tiles can no longer stall those.
    let source = TileSource::default();
    let (tile_tx, tile_rx) = mpsc::channel::<TileKey>();
    let tile_rx = Arc::new(Mutex::new(tile_rx));
    let workers = std::thread::available_parallelism()
        .map(|count| count.get())
        .unwrap_or(1)
        .clamp(1, MAX_TILE_WORKERS);
    for _ in 0..workers {
        let tile_rx = tile_rx.clone();
        let sender = sender.clone();
        let wake = wake.clone();
        let source = source.clone();
        std::thread::spawn(move || loop {
            // One worker at a time waits on the shared receiver; it releases
            // the lock as soon as it has a key, so the others pick up the next
            // ones while it renders. Keys come out in the centre-out order the
            // batch was queued in. `the_tile_pool_renders_several_tiles_
            // concurrently` covers the overlap this depends on.
            let key = {
                let Ok(rx) = tile_rx.lock() else { break };
                match rx.recv() {
                    Ok(key) => key,
                    Err(_) => break,
                }
            };
            let result = match source.get() {
                Some(document) => document
                    .accepts_tile(&key)
                    .and_then(|()| document.page(key.page))
                    .and_then(|page| document.tile_from(&page, key))
                    .map_err(|error| format!("{error:#}")),
                None => Err("Open a document first".to_owned()),
            };
            if sender.send(Event::Tile(result)).is_err() {
                break;
            }
            wake();
        });
    }
    let workers = Workers {
        thumb_tx,
        tile_tx,
        source,
        offline_tx,
        policy,
        busy,
    };
    std::thread::spawn(move || {
        let mut document: Option<OpenedDocument> = None;
        // The one page we may fetch and render ahead, never more.
        let mut prefetch: Option<usize> = None;
        loop {
            // Take a queued command, or, when nothing is waiting, spend the
            // idle moment on the single page of prefetch. Any user action
            // arriving meanwhile is served first, because the prefetch step
            // ends before the next receive.
            let command = match receiver.try_recv() {
                Ok(command) => Some(command),
                Err(mpsc::TryRecvError::Empty) => None,
                Err(mpsc::TryRecvError::Disconnected) => break,
            };
            let Some(command) = command else {
                if let Some(index) = prefetch.take() {
                    if let Some(opened) = document.as_mut() {
                        // Best effort: a prefetch failure is the page the user
                        // has not asked for yet, so it is never reported.
                        workers.busy.store(true, Ordering::Relaxed);
                        let _ = opened.prefetch(index);
                        workers.busy.store(false, Ordering::Relaxed);
                    }
                    continue;
                }
                match receiver.recv() {
                    Ok(command) => {
                        handle(
                            command,
                            &mut document,
                            &mut prefetch,
                            &sender,
                            &wake,
                            &workers,
                        );
                        continue;
                    }
                    Err(_) => break,
                }
            };
            handle(
                command,
                &mut document,
                &mut prefetch,
                &sender,
                &wake,
                &workers,
            );
        }
    });
    (commands, events)
}

/// Dispatch one command: background rendering to its own worker, everything
/// else to the serialized foreground path.
fn handle(
    command: Command,
    document: &mut Option<OpenedDocument>,
    prefetch: &mut Option<usize>,
    sender: &Sender<Event>,
    wake: &Arc<impl Fn() + Send + Sync + 'static>,
    workers: &Workers,
) {
    match command {
        Command::Thumbnail { id, epoch } => {
            if let Err(error) = workers.thumb_tx.try_send((id, epoch)) {
                let (id, epoch) = match error {
                    mpsc::TrySendError::Full(value) | mpsc::TrySendError::Disconnected(value) => {
                        value
                    }
                };
                let _ = sender.send(Event::Thumbnail {
                    id,
                    epoch,
                    preview: Err("Thumbnail worker is busy".into()),
                });
                wake();
            }
            return;
        }
        Command::Tiles(keys) => {
            // Bands the tiles share start rendering together, rather than
            // one after another as the workers reach them.
            if let Some(document) = workers.source.get() {
                document.warm_bands(&keys);
            }
            // Already ordered centre-out by the viewer; the pool preserves it.
            for key in keys {
                if workers.tile_tx.send(key).is_err() {
                    let _ = sender.send(Event::Tile(Err("The renderer stopped.".into())));
                    wake();
                    break;
                }
            }
            return;
        }
        // Settings work never waits behind a download, and never makes the
        // library look busy: it runs on a short-lived thread of its own.
        Command::SetCacheMode(mode) => {
            workers.policy.set(mode);
            let sender = sender.clone();
            let wake = wake.clone();
            if mode == CacheMode::Full {
                let _ = workers.offline_tx.send(());
            } else {
                let _ = sender.send(Event::Offline(Offline::Idle));
            }
            std::thread::spawn(move || {
                if let Ok(cache) = library::default_cache_dir() {
                    if mode != CacheMode::Full {
                        let _ = library::trim_component_cache(&cache, mode.budget());
                    }
                    let _ = sender.send(Event::Storage(library::component_cache_bytes(&cache)));
                }
                wake();
            });
            return;
        }
        Command::MeasureStorage => {
            let sender = sender.clone();
            let wake = wake.clone();
            std::thread::spawn(move || {
                let Ok(cache) = library::default_cache_dir() else {
                    return;
                };
                let _ = sender.send(Event::Storage(library::component_cache_bytes(&cache)));
                wake();
                // The library's size needs the cloud listing; offline, the
                // dialog simply does not show it.
                if let Ok(plan) = offline_plan(&cache) {
                    let _ = sender.send(Event::LibrarySize(plan.library_bytes));
                    wake();
                }
            });
            return;
        }
        Command::ClearCache => {
            let sender = sender.clone();
            let wake = wake.clone();
            std::thread::spawn(move || {
                let result = library::default_cache_dir().and_then(|cache| {
                    let freed = library::prune_component_cache(&cache, &HashSet::new())?;
                    Ok((freed, library::component_cache_bytes(&cache)))
                });
                let _ = sender.send(match result {
                    Ok((freed, used)) => {
                        let _ = sender.send(Event::Storage(used));
                        Event::Message(format!(
                            "Removed {} of downloaded documents",
                            format_bytes(freed)
                        ))
                    }
                    Err(error) => Event::Error(format!("{error:#}")),
                });
                wake();
            });
            return;
        }
        _ => {}
    }
    // A page or document change makes any queued prefetch the wrong page.
    if matches!(command, Command::Open { .. } | Command::Page(_)) {
        *prefetch = None;
    }
    // Anything that lists the library again may have changed what the full
    // library download has to fetch.
    let relists = matches!(
        command,
        Command::Connect
            | Command::Pair(_)
            | Command::Refresh
            | Command::Import { .. }
            | Command::Move { .. }
            | Command::CreateFolder { .. }
    );
    workers.busy.store(true, Ordering::Relaxed);
    let result = execute(command, document, sender, &workers.policy);
    workers.busy.store(false, Ordering::Relaxed);
    match result {
        Ok(()) if relists && workers.policy.mode() == CacheMode::Full => {
            let _ = workers.offline_tx.send(());
        }
        Ok(()) => {}
        Err(error) => {
            let _ = sender.send(Event::Error(format!("{error:#}")));
        }
    }
    // Publish whatever document is open now, so tile workers never hold a
    // reference to a document the foreground worker has replaced.
    workers
        .source
        .set(document.as_ref().map(|opened| opened.document.clone()));
    // Queue exactly one page of read-ahead, and only when the page after the
    // one now shown exists and is not already rendered.
    *prefetch = document.as_ref().and_then(|opened| opened.next_page());
    if sender.send(Event::Finished).is_err() {
        return;
    }
    wake();
}

struct Session {
    http: turborm_core::cloud::Http,
    token: Zeroizing<String>,
    host: String,
}
impl Session {
    fn connect() -> Result<Self> {
        let (http, token, host) = auth::connect()?;
        Ok(Self {
            http,
            token: Zeroizing::new(token),
            host,
        })
    }
    fn library(&self) -> Result<Library<'_, turborm_core::cloud::Http>> {
        Ok(Library::new(
            &self.http,
            &self.host,
            &self.token,
            Some(library::default_cache_dir()?),
        ))
    }
}

/// Keep a verified immutable snapshot for lazy page downloads. The renderer
/// owns the private directory and has no access to credentials or HTTP.
struct OpenedDocument {
    /// Shared with the tile workers, which render from the same page cache.
    document: Arc<Document>,
    session: Session,
    snapshot: library::Snapshot,
    downloads: PageDownloads,
    /// The page the viewer is showing, which fixes what may be prefetched.
    shown: usize,
    /// The page the tablet last showed, where the viewer opens.
    last_read: usize,
    /// How much the disk cache may keep; read at each download.
    policy: Policy,
}

struct PageDownloads {
    bundle: PathBuf,
    id: String,
    available: HashSet<String>,
    bytes: usize,
}

impl PageDownloads {
    fn component(
        &mut self,
        name: &str,
        limit: usize,
        fetch: &mut impl FnMut(&str, usize) -> Result<Vec<u8>>,
    ) -> Result<()> {
        if !self.available.contains(name) || self.bundle.join(name).is_file() {
            return Ok(());
        }
        let remaining = (512 * 1024 * 1024usize).saturating_sub(self.bytes);
        ensure!(remaining > 0, "Preview download exceeds 512 MiB");
        let bytes = fetch(name, limit.min(remaining))?;
        ensure!(
            bytes.len() <= limit.min(remaining),
            "Preview component exceeds download limit"
        );
        let destination = self.bundle.join(name);
        let mut output = tempfile::NamedTempFile::new_in(
            destination
                .parent()
                .ok_or_else(|| anyhow!("Invalid component path"))?,
        )?;
        use std::io::Write;
        output.write_all(&bytes)?;
        output
            .persist_noclobber(&destination)
            .map_err(|_| anyhow!("Could not save verified preview component"))?;
        self.bytes += bytes.len();
        Ok(())
    }

    fn page(
        &mut self,
        pages: &[turborm_core::notebook::PageRef],
        index: usize,
        fetch: &mut impl FnMut(&str, usize) -> Result<Vec<u8>>,
    ) -> Result<()> {
        let page = pages
            .get(index)
            .ok_or_else(|| anyhow!("Page is out of range"))?;
        if !page.id.is_empty() {
            let name = format!("{}/{}.rm", self.id, page.id);
            self.component(&name, SCENE_LIMIT, fetch)?;
            self.images(&page.id, fetch)?;
        }
        Ok(())
    }

    /// Fetch the inserted images the page's scene names, beside it in
    /// `<document>/<page>/`, through the same bounded and verified component
    /// path. Each file is limited to [`notebook::MAX_IMAGE_BYTES`] and the
    /// page's images together to [`notebook::MAX_PAGE_IMAGE_BYTES`]. A file
    /// that cannot be fetched is left out; the renderer reports it as missing
    /// and a later visit retries it.
    fn images(
        &mut self,
        page: &str,
        fetch: &mut impl FnMut(&str, usize) -> Result<Vec<u8>>,
    ) -> Result<()> {
        use turborm_core::notebook;
        let scene = self.bundle.join(&self.id).join(format!("{page}.rm"));
        if !scene.is_file() {
            return Ok(());
        }
        let bytes = job::read_bounded(&scene, 64 * 1024 * 1024)?;
        // An unreadable scene reports its own error when it is rendered.
        let Ok(names) = notebook::image_files(&bytes) else {
            return Ok(());
        };
        let mut page_bytes = 0usize;
        for name in names {
            let path = format!("{}/{page}/{name}", self.id);
            if !self.available.contains(&path) {
                continue;
            }
            let destination = self.bundle.join(&path);
            if let Ok(existing) = std::fs::metadata(&destination) {
                page_bytes = page_bytes.saturating_add(existing.len() as usize);
                continue;
            }
            let limit = notebook::MAX_IMAGE_BYTES
                .min(notebook::MAX_PAGE_IMAGE_BYTES.saturating_sub(page_bytes));
            if limit == 0 {
                break;
            }
            if let Some(parent) = destination.parent() {
                std::fs::create_dir_all(parent)?;
            }
            if self.component(&path, limit, fetch).is_ok() {
                if let Ok(saved) = std::fs::metadata(&destination) {
                    page_bytes = page_bytes.saturating_add(saved.len() as usize);
                }
            }
        }
        Ok(())
    }
}

impl OpenedDocument {
    fn load(
        session: Session,
        snapshot: library::Snapshot,
        info: library::DocumentInfo,
        source_hash: String,
        policy: Policy,
        last_read: usize,
    ) -> Result<Self> {
        let id = info.entry.id.clone();
        let content = info
            .content
            .as_ref()
            .ok_or_else(|| anyhow!("Document has no content settings"))?;
        let pages = turborm_core::notebook::pages(content)?;
        ensure!(
            !pages.is_empty(),
            "No renderable pages yet. Let the tablet sync its pages, then refresh."
        );
        let directory = tempfile::Builder::new()
            .prefix("remarkable-view-")
            .tempdir()?;
        let bundle = directory.path().join("bundle");
        std::fs::create_dir(&bundle)?;
        std::fs::create_dir(bundle.join(&id))?;
        std::fs::write(
            bundle.join(format!("{id}.content")),
            serde_json::to_vec(content)?,
        )?;
        std::fs::write(
            bundle.join(format!("{id}.metadata")),
            serde_json::to_vec(&info.metadata)?,
        )?;
        let mut downloads = PageDownloads {
            bundle,
            id: id.clone(),
            available: info.files.into_iter().map(|file| file.path).collect(),
            bytes: 0,
        };
        let lib = session.library()?;
        let budget = policy.budget();
        let mut fetch =
            |name: &str, limit| lib.cached_component(&snapshot, &id, name, limit, budget);
        // A shared PDF is needed by background pages; unrelated EPUB/source
        // components and all other .rm pages can stay in the cloud for now.
        downloads.component(&format!("{id}.pdf"), PDF_LIMIT, &mut fetch)?;
        if content["formatVersion"] == 1 {
            downloads.component(&format!("{id}.pagedata"), PAGEDATA_LIMIT, &mut fetch)?;
        }
        let last_read = if last_read < pages.len() {
            last_read
        } else {
            0
        };
        downloads.page(&pages, last_read, &mut fetch)?;
        let mut document = Document::load(directory, id, info.entry.title)?;
        document.source_hash = Some(source_hash);
        Ok(Self {
            document: Arc::new(document),
            session,
            snapshot,
            downloads,
            shown: last_read,
            last_read,
            policy,
        })
    }

    fn ensure_page(&mut self, index: usize) -> Result<()> {
        let lib = self.session.library()?;
        let id = self.downloads.id.clone();
        let budget = self.policy.budget();
        self.downloads
            .page(&self.document.pages, index, &mut |name, limit| {
                lib.cached_component(&self.snapshot, &id, name, limit, budget)
            })
    }

    /// The single page worth reading ahead: the one after the page shown, and
    /// only while it exists and is not already rendered.
    fn next_page(&self) -> Option<usize> {
        next_prefetch(&self.document, self.shown)
    }

    /// Download and render one page ahead of the viewer.
    ///
    /// Runs only when no command is waiting, so a user action is always served
    /// before the read-ahead resumes.
    fn prefetch(&mut self, index: usize) -> Result<()> {
        ensure!(index < self.document.pages.len(), "Page is out of range");
        self.ensure_page(index)?;
        self.document.prefetch_page(index)
    }
}

/// What the full-library download keeps and still has to fetch.
struct OfflinePlan {
    /// Hashes of every document file the viewer may read.
    keep: HashSet<String>,
    /// `(document, file, bytes, download limit)` for files not on disk yet.
    missing: Vec<(String, String, u64, usize)>,
    /// Bytes every document's files take together.
    library_bytes: u64,
    /// Documents whose file list could not be read. Pruning waits until
    /// there are none, so nothing is removed on the strength of a guess.
    unreadable: usize,
}

/// The download limit for `path` when the viewer may read it for document
/// `id`, or `None` when it never does (EPUB sources, thumbnails, metadata).
fn offline_limit(id: &str, path: &str) -> Option<usize> {
    if path.strip_prefix(id) == Some(".pdf") {
        return Some(PDF_LIMIT);
    }
    if path.strip_prefix(id) == Some(".pagedata") {
        return Some(PAGEDATA_LIMIT);
    }
    let inner = path.strip_prefix(id)?.strip_prefix('/')?;
    match inner.split('/').count() {
        1 if inner.ends_with(".rm") => Some(SCENE_LIMIT),
        // Images inserted on a page, in `<document>/<page>/`.
        2 => Some(turborm_core::notebook::MAX_IMAGE_BYTES),
        _ => None,
    }
}

fn plan_offline(
    library: &Library<'_, turborm_core::cloud::Http>,
    snapshot: &library::Snapshot,
    cache: &Path,
) -> Result<OfflinePlan> {
    let listing = library.list(snapshot, 4)?;
    let mut plan = OfflinePlan {
        keep: HashSet::new(),
        missing: Vec::new(),
        library_bytes: 0,
        unreadable: listing.failures.len(),
    };
    for entry in listing.entries {
        if entry.kind != "document" || entry.deleted || entry.parent == "trash" {
            continue;
        }
        let Ok(rows) = library.document_rows(snapshot, &entry.id) else {
            plan.unreadable += 1;
            continue;
        };
        for row in rows {
            let Some(limit) = offline_limit(&entry.id, &row.name) else {
                continue;
            };
            if row.size > limit as u64 || !plan.keep.insert(row.hash.clone()) {
                continue;
            }
            plan.library_bytes += row.size;
            if !library::is_component_cached(cache, &row.hash, row.size) {
                plan.missing
                    .push((entry.id.clone(), row.name, row.size, limit));
            }
        }
    }
    Ok(plan)
}

/// [`plan_offline`] against a fresh cloud snapshot.
fn offline_plan(cache: &Path) -> Result<OfflinePlan> {
    let session = Session::connect()?;
    let library = session.library()?;
    let snapshot = library.snapshot()?;
    plan_offline(&library, &snapshot, cache)
}

/// Download every document file the viewer may read into the disk cache,
/// and remove the files no document uses any more.
///
/// Stands aside while the foreground worker is busy, stops as soon as the
/// mode is no longer "full", and never leaves less than
/// [`OFFLINE_FREE_SPACE_RESERVE`] of the disk free.
fn save_library_offline(
    policy: &Policy,
    busy: &AtomicBool,
    report: &mut impl FnMut(Offline),
) -> Result<Offline> {
    let cache = library::default_cache_dir()?;
    let session = Session::connect()?;
    let library = session.library()?;
    let snapshot = library.snapshot()?;
    let plan = plan_offline(&library, &snapshot, &cache)?;
    // Old versions of edited pages and deleted documents go first, so the
    // space they held is there for the download.
    if plan.unreadable == 0 {
        library::prune_component_cache(&cache, &plan.keep)?;
    }
    let total = plan.missing.iter().map(|(_, _, size, _)| size).sum();
    let mut done = 0;
    let mut failed = plan.unreadable;
    let mut last_error = None;
    let mut reported = Instant::now();
    if total > 0 {
        report(Offline::Saving { done, total });
    }
    for (id, path, size, limit) in plan.missing {
        while busy.load(Ordering::Relaxed) && policy.mode() == CacheMode::Full {
            std::thread::sleep(Duration::from_millis(100));
        }
        if policy.mode() != CacheMode::Full {
            return Ok(Offline::Idle);
        }
        if free_bytes(&cache).is_some_and(|free| free < size + OFFLINE_FREE_SPACE_RESERVE) {
            return Ok(Offline::Stopped(format!(
                "Paused to keep {} of disk free",
                format_bytes(OFFLINE_FREE_SPACE_RESERVE)
            )));
        }
        if let Err(error) = library.cached_component(&snapshot, &id, &path, limit, u64::MAX) {
            failed += 1;
            last_error = Some(error);
        }
        done += size;
        if reported.elapsed() >= Duration::from_millis(250) {
            report(Offline::Saving { done, total });
            reported = Instant::now();
        }
    }
    if failed > 0 {
        let reason = last_error.map_or_else(String::new, |error| format!(": {error:#}"));
        return Ok(Offline::Stopped(format!(
            "{failed} file{} could not be saved; Refresh retries{reason}",
            if failed == 1 { "" } else { "s" }
        )));
    }
    Ok(Offline::Ready {
        library: plan.library_bytes,
    })
}

/// Bytes free for this user on the file system holding `path`.
fn free_bytes(path: &Path) -> Option<u64> {
    let existing = path.ancestors().find(|ancestor| ancestor.exists())?;
    let stats = rustix::fs::statvfs(existing).ok()?;
    Some(stats.f_bavail.saturating_mul(stats.f_frsize))
}

/// "12 KB", "450 MB", "2.3 GB": sizes for people, in binary units.
pub fn format_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    let value = bytes as f64;
    if value >= KIB * KIB * KIB {
        format!("{:.1} GB", value / (KIB * KIB * KIB))
    } else if value >= KIB * KIB {
        format!("{:.0} MB", value / (KIB * KIB))
    } else {
        format!("{:.0} KB", value / KIB)
    }
}

/// The one page a viewer showing `shown` may read ahead.
///
/// Exactly one page, never two, and nothing at all when the next page is
/// already rendered or does not exist.
fn next_prefetch(document: &Document, shown: usize) -> Option<usize> {
    let next = shown.checked_add(1)?;
    (next < document.pages.len() && !document.is_page_cached(next)).then_some(next)
}

fn list(sender: &Sender<Event>) -> Result<()> {
    let session = Session::connect()?;
    let library = session.library()?;
    let snapshot = library.snapshot()?;
    let mut listing = library.list(&snapshot, 4)?;
    listing.entries.retain(|entry| !entry.deleted);
    if !listing.failures.is_empty() {
        let examples = listing
            .failures
            .iter()
            .take(3)
            .map(|failure| format!("{}: {}", failure.id, failure.error))
            .collect::<Vec<_>>()
            .join("; ");
        let more = listing.failures.len().saturating_sub(3);
        let suffix = if more == 0 {
            String::new()
        } else {
            format!("; and {more} more")
        };
        let _ = sender.send(Event::Message(format!(
            "Loaded {} entries; skipped {} unreadable cloud entr{}: {examples}{suffix}. Refresh to retry.",
            listing.entries.len(),
            listing.failures.len(),
            if listing.failures.len() == 1 { "y" } else { "ies" },
        )));
    }
    let _ = sender.send(Event::Listing(listing.entries));
    Ok(())
}

fn execute(
    command: Command,
    document: &mut Option<OpenedDocument>,
    sender: &Sender<Event>,
    policy: &Policy,
) -> Result<()> {
    match command {
        Command::Tiles(_)
        | Command::Thumbnail { .. }
        | Command::SetCacheMode(_)
        | Command::MeasureStorage
        | Command::ClearCache => {
            unreachable!("Background rendering is dispatched separately")
        }
        Command::Connect => {
            let paired =
                auth::has_credential().context("Unlock your desktop keyring and try again")?;
            let _ = sender.send(Event::Paired(paired));
            if paired {
                list(sender)?;
            }
        }
        Command::Pair(code) => {
            if let Err(error) = auth::pair_with_code(&code, false) {
                // Registration can succeed before token renewal fails. Expose
                // the saved state so retry reconnects instead of re-registering.
                if auth::has_credential().unwrap_or(false) {
                    let _ = sender.send(Event::Paired(true));
                }
                return Err(error);
            }
            let _ = sender.send(Event::Paired(true));
            list(sender)?;
        }
        Command::Refresh => list(sender)?,
        Command::Open { id } => {
            let _ = sender.send(Event::Message("Opening…".into()));
            let session = Session::connect()?;
            let library = session.library()?;
            let snapshot = library.snapshot()?;
            let source_hash = snapshot
                .manifest
                .rows
                .iter()
                .find(|row| row.name == id)
                .map(|row| row.hash.clone())
                .ok_or_else(|| anyhow!("Document is no longer in the library"))?;
            if let Some(opened) = document
                .as_mut()
                .filter(|opened| opened.document.source_hash.as_deref() == Some(&source_hash))
            {
                // The same document, already downloaded: switch screens with
                // its title at once and repaint from the page cache.
                // An unchanged document hash means the tablet has not moved
                // on since it was loaded, so its last page still stands.
                let _ = sender.send(Event::Opening {
                    title: opened.document.title.clone(),
                    pages: opened.document.pages.len(),
                    page: opened.last_read,
                });
                opened.session = session;
                opened.snapshot = snapshot;
                opened.shown = opened.last_read;
                let preview = opened.document.preview(opened.last_read)?;
                let _ = sender.send(Event::Preview(preview));
                return Ok(());
            }
            let info = library.info(&snapshot, &id)?;
            ensure!(
                info.entry.kind == "document" && !info.entry.deleted,
                "Select an active document"
            );
            // Switch the viewer before anything is downloaded, so the title,
            // the page count and the cover placeholder appear immediately.
            let (pages, last_read) = info
                .content
                .as_ref()
                .and_then(|content| {
                    let pages = turborm_core::notebook::pages(content).ok()?;
                    let last_read =
                        turborm_core::cover::last_opened_page(content, &info.metadata, &pages);
                    Some((pages.len(), last_read.unwrap_or(0)))
                })
                .unwrap_or((0, 0));
            let _ = sender.send(Event::Opening {
                title: info.entry.title.clone(),
                pages,
                page: last_read,
            });
            // The manifest knows what has to come down; say so once rather
            // than leaving a long PDF download unexplained. Only the PDF is
            // downloaded on opening, and only when the disk cache lacks it.
            let cache = library::default_cache_dir()?;
            let bytes: u64 = info
                .files
                .iter()
                .filter(|file| file.path == format!("{id}.pdf"))
                .filter(|file| !library::is_component_cached(&cache, &file.sha256, file.bytes))
                .map(|file| file.bytes)
                .sum();
            if bytes >= 1024 * 1024 {
                let _ = sender.send(Event::Message(format!(
                    "Downloading {:.1} MB…",
                    bytes as f64 / (1024.0 * 1024.0)
                )));
            }
            let opened = OpenedDocument::load(
                session,
                snapshot,
                info,
                source_hash,
                policy.clone(),
                last_read,
            )?;
            let _ = sender.send(Event::Message(format!(
                "Rendering page {}…",
                opened.last_read + 1
            )));
            let preview = opened.document.preview(opened.last_read)?;
            *document = Some(opened);
            let _ = sender.send(Event::Preview(preview));
        }
        Command::Page(page) => {
            let opened = document
                .as_mut()
                .ok_or_else(|| anyhow!("Open a document first"))?;
            // A cached page repaints without a download or a render, so say
            // nothing; otherwise name the page that is being prepared.
            if !opened.document.is_page_cached(page) {
                let _ = sender.send(Event::Message(format!("Rendering page {}…", page + 1)));
            }
            opened.ensure_page(page)?;
            let preview = opened.document.preview(page)?;
            opened.shown = page;
            let _ = sender.send(Event::Preview(preview));
        }
        Command::Import { parent } => {
            if let Some(input) = rfd::FileDialog::new()
                .set_title("Import into reMarkable")
                .add_filter(
                    "Documents and images",
                    &["pdf", "epub", "png", "jpg", "jpeg"],
                )
                .pick_file()
            {
                let session = Session::connect()?;
                validate_parent(&session, &parent)?;
                let output = job::default_job_dir()?;
                import::prepare_import(&input, &output, None, &parent)?;
                hash_tree::send(&session.http, &session.host, &session.token, &output)
                    .with_context(|| {
                        format!(
                            "Import journal: {}. Use Check import before trying another upload",
                            output.display()
                        )
                    })?;
                let _ = sender.send(Event::Message("Imported and verified in the cloud.".into()));
                list(sender).context("Import succeeded, but refreshing the library failed")?;
            }
        }
        Command::Move { id, parent } => {
            let session = Session::connect()?;
            moves::move_entry(&session.http, &session.host, &session.token, &id, &parent)?;
            let _ = sender.send(Event::Message("Moved.".into()));
            list(sender).context("Move succeeded, but refreshing the library failed")?;
        }
        Command::CreateFolder { parent, name } => {
            let session = Session::connect()?;
            let id = folders::create_folder(
                &session.http,
                &session.host,
                &session.token,
                Some(library::default_cache_dir()?),
                &parent,
                &name,
            )?;
            list(sender).context("The folder was created, but refreshing the library failed")?;
            let _ = sender.send(Event::Message("Folder created".into()));
            let _ = sender.send(Event::FolderCreated(id));
        }
        Command::ExportOriginal { id } => {
            let session = Session::connect()?;
            let library = session.library()?;
            let snapshot = library.snapshot()?;
            let info = library.info(&snapshot, &id)?;
            let ext = info
                .content
                .as_ref()
                .and_then(|c| c["fileType"].as_str())
                .unwrap_or("");
            ensure!(matches!(ext, "pdf" | "epub"), "This notebook has no original PDF/EPUB. Open it to export a preview PDF or page SVG, or export its native bundle.");
            if let Some(out) = save_dialog(&info.entry.title, ext) {
                library.export(&snapshot, &id, &out, library::DEFAULT_MAX_BYTES)?;
                message_path(sender, &out);
            }
        }
        Command::ExportNative { id } => {
            let session = Session::connect()?;
            let library = session.library()?;
            let snapshot = library.snapshot()?;
            let info = library.info(&snapshot, &id)?;
            if let Some(parent) = rfd::FileDialog::new()
                .set_title("Choose where to save the native bundle")
                .pick_folder()
            {
                let out = parent.join(format!("{}-{}", filename(&info.entry.title), id));
                library.pull(&snapshot, &id, &out, library::DEFAULT_MAX_BYTES)?;
                message_path(sender, &out);
            }
        }
        Command::ExportPdf => {
            let opened = document
                .as_mut()
                .ok_or_else(|| anyhow!("Open a document first"))?;
            if let Some(out) = save_dialog(&opened.document.title, "pdf") {
                export_preview_pdf(opened, &out, sender)?;
            }
        }
        Command::ExportSvg(index) => {
            let doc = current(document)?;
            if let Some(out) = save_dialog(&format!("{}-page-{}", doc.title, index + 1), "svg") {
                doc.export_svg(index, &out)?;
                message_path(sender, &out);
            }
        }
        Command::ExportSvgPages => {
            let opened = document
                .as_mut()
                .ok_or_else(|| anyhow!("Open a document first"))?;
            if let Some(out) = svg_destination(&opened.document.title, opened.document.pages.len())
            {
                export_svg_pages(opened, out, sender)?;
            }
        }
        Command::ExportDocument { id, format } => {
            let session = Session::connect()?;
            let library = session.library()?;
            let snapshot = library.snapshot()?;
            let source_hash = snapshot
                .manifest
                .rows
                .iter()
                .find(|row| row.name == id)
                .map(|row| row.hash.clone())
                .ok_or_else(|| anyhow!("Document is no longer in the library"))?;
            let info = library.info(&snapshot, &id)?;
            ensure!(
                info.entry.kind == "document" && !info.entry.deleted,
                "Select an active document"
            );
            let pages = info
                .content
                .as_ref()
                .and_then(|content| turborm_core::notebook::pages(content).ok())
                .map_or(0, |pages| pages.len());
            ensure!(
                pages > 0,
                "No renderable pages yet. Let the tablet sync its pages, then refresh."
            );
            // Ask where to save before anything is downloaded.
            let title = info.entry.title.clone();
            let out = match format {
                ExportFormat::PreviewPdf => save_dialog(&title, "pdf").map(ExportOut::Pdf),
                ExportFormat::Svg => svg_destination(&title, pages).map(ExportOut::Svg),
            };
            let Some(out) = out else {
                return Ok(());
            };
            // The viewer's copy is reused when it is this very version;
            // otherwise a private copy is loaded and dropped afterwards, so
            // the viewer keeps whatever it has open.
            let mut loaded = None;
            let opened = match document
                .as_mut()
                .filter(|opened| opened.document.source_hash.as_deref() == Some(&source_hash))
            {
                Some(opened) => opened,
                None => {
                    let _ = sender.send(Event::Message(format!("Downloading “{title}”…")));
                    loaded.insert(OpenedDocument::load(
                        session,
                        snapshot,
                        info,
                        source_hash,
                        policy.clone(),
                        0,
                    )?)
                }
            };
            match out {
                ExportOut::Pdf(out) => export_preview_pdf(opened, &out, sender)?,
                ExportOut::Svg(out) => export_svg_pages(opened, out, sender)?,
            }
        }
        Command::VerifyImport => {
            if let Some(path) = rfd::FileDialog::new()
                .set_title("Choose an import journal folder")
                .pick_folder()
            {
                let session = Session::connect()?;
                hash_tree::verify(&session.http, &session.host, &session.token, &path)?;
                let _ = sender.send(Event::Message("Import verified in the cloud.".into()));
                list(sender)?;
            }
        }
    }
    Ok(())
}

/// Fetch just the selected cover scene and its background, not the whole
/// notebook. The immutable document hash invalidates covers after any edit,
/// including a change to the chosen cover or last-opened page.
fn thumbnail_context() -> Result<(Session, library::Snapshot)> {
    let session = Session::connect()?;
    let snapshot = session.library()?.snapshot()?;
    Ok((session, snapshot))
}

fn thumbnail(
    session: &Session,
    snapshot: &library::Snapshot,
    id: &str,
    budget: u64,
) -> Result<Preview> {
    let library = session.library()?;
    let row = snapshot
        .manifest
        .rows
        .iter()
        .find(|row| row.name == id)
        .ok_or_else(|| anyhow!("Document is no longer in the library"))?;
    let info = library.info(snapshot, id)?;
    ensure!(
        info.entry.kind == "document" && !info.entry.deleted,
        "No document cover"
    );
    let content = info
        .content
        .as_ref()
        .ok_or_else(|| anyhow!("Document has no page settings"))?;
    let pages = turborm_core::notebook::pages(content)?;
    let index = turborm_core::cover::page_index(content, &info.metadata, &pages)
        .ok_or_else(|| anyhow!("Document has no visible pages"))?;
    // Bump the directory name whenever rendering geometry changes so covers
    // rendered by an older placement model are never reused.
    let cache_dir = library::default_cache_dir()?.join("covers-v5");
    private_directory(&cache_dir)?;
    let cached_path = cache_dir.join(format!("{}.png", row.hash));
    // Local template assets may change independently of the cloud document.
    let use_cache = std::env::var_os("REMARKABLE_TEMPLATES_DIR").is_none();
    if let Ok(bytes) = if use_cache {
        job::read_bounded(&cached_path, 2 * 1024 * 1024)
    } else {
        Err(anyhow!("Local templates are enabled"))
    } {
        let mut reader =
            image::ImageReader::with_format(std::io::Cursor::new(&bytes), image::ImageFormat::Png);
        let mut limits = image::Limits::default();
        limits.max_image_width = Some(280);
        limits.max_image_height = Some(280);
        limits.max_alloc = Some(4 * 1024 * 1024);
        reader.limits(limits);
        if let Ok(image) = reader.decode() {
            return Ok(Preview {
                width: image.width() as usize,
                height: image.height() as usize,
                canvas_width: image.width() as f32,
                canvas_height: image.height() as f32,
                rgba: image.to_rgba8().into_raw(),
                page: index,
                total: pages.len(),
                title: info.entry.title,
                document_key: row.hash.clone(),
                warnings: Vec::new(),
            });
        }
    }
    let directory = tempfile::Builder::new()
        .prefix("remarkable-cover-")
        .tempdir()?;
    let bundle = directory.path().join("bundle");
    std::fs::create_dir(&bundle)?;
    std::fs::create_dir(bundle.join(id))?;
    std::fs::write(
        bundle.join(format!("{id}.content")),
        serde_json::to_vec(content)?,
    )?;
    std::fs::write(
        bundle.join(format!("{id}.metadata")),
        serde_json::to_vec(&info.metadata)?,
    )?;
    let scene_name = format!("{id}/{}.rm", pages[index].id);
    let pagedata_name = format!("{id}.pagedata");
    if content["formatVersion"] == 1 && info.files.iter().any(|file| file.path == pagedata_name) {
        let bytes = library.component(snapshot, id, &pagedata_name, 4 * 1024 * 1024)?;
        std::fs::write(bundle.join(&pagedata_name), bytes)?;
    }
    if info.files.iter().any(|file| file.path == scene_name) {
        let bytes = library.component(snapshot, id, &scene_name, 64 * 1024 * 1024)?;
        std::fs::write(bundle.join(&scene_name), bytes)?;
        // The cover page's inserted images, bounded like the viewer's.
        let mut downloads = PageDownloads {
            bundle: bundle.clone(),
            id: id.to_owned(),
            available: info.files.iter().map(|file| file.path.clone()).collect(),
            bytes: 0,
        };
        downloads.images(&pages[index].id, &mut |name, limit| {
            library.component(snapshot, id, name, limit)
        })?;
    }
    let pdf_name = format!("{id}.pdf");
    if pages[index].background.is_some() && info.files.iter().any(|file| file.path == pdf_name) {
        // Through the viewer's disk cache and with its limit: a PDF kept on
        // disk makes its cover at once, and a large one is downloaded once
        // rather than never, since only the small cover is kept.
        let bytes = library.cached_component(snapshot, id, &pdf_name, PDF_LIMIT, budget)?;
        std::fs::write(bundle.join(&pdf_name), bytes)?;
    }
    let document = Document::load_for_cover(directory, id.to_owned(), info.entry.title)?;
    let preview = document.cover(document.cover_page)?;
    // Cache only the small rendered cover, under a private directory. Source
    // components are discarded with the temporary document.
    let mut output = tempfile::NamedTempFile::new_in(&cache_dir)?;
    image::write_buffer_with_format(
        output.as_file_mut(),
        &preview.rgba,
        preview.width as u32,
        preview.height as u32,
        image::ColorType::Rgba8,
        image::ImageFormat::Png,
    )?;
    output.as_file().sync_all()?;
    if use_cache {
        let _ = output.persist(&cached_path);
    }
    Ok(preview)
}

fn private_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    #[cfg(not(unix))]
    std::fs::create_dir_all(path)?;
    Ok(())
}

fn current(document: &Option<OpenedDocument>) -> Result<&Document> {
    document
        .as_ref()
        .map(|opened| opened.document.as_ref())
        .ok_or_else(|| anyhow!("Open a document first"))
}
fn validate_parent(session: &Session, parent: &str) -> Result<()> {
    if parent.is_empty() {
        return Ok(());
    }
    let library = session.library()?;
    let snapshot = library.snapshot()?;
    let info = library.info(&snapshot, parent)?;
    ensure!(
        info.entry.kind == "folder"
            && !info.entry.deleted
            && info.entry.hierarchy_warning.is_none(),
        "Import destination is no longer an active folder. Refresh and choose another folder"
    );
    Ok(())
}
fn filename(title: &str) -> String {
    let name: String = title
        .chars()
        .filter(|c| !c.is_control() && !"/\\<>:\"|?*".contains(*c))
        .take(120)
        .collect();
    let name = name.trim().trim_matches('.');
    if name.is_empty() {
        "document".into()
    } else {
        name.into()
    }
}
fn save_dialog(title: &str, ext: &str) -> Option<PathBuf> {
    rfd::FileDialog::new()
        .set_title("Export to a new file")
        .set_file_name(format!("{}.{ext}", filename(title)))
        .add_filter(ext.to_uppercase(), &[ext])
        .save_file()
}
fn message_path(sender: &Sender<Event>, path: &Path) {
    let _ = sender.send(Event::Message(format!("Saved to {}", path.display())));
}

/// Where a library export goes, chosen before anything is downloaded.
enum ExportOut {
    Pdf(PathBuf),
    Svg(SvgOut),
}

enum SvgOut {
    /// A single-page document: one .svg file.
    File(PathBuf),
    /// A folder named after the document, made at export time, that gets
    /// page1.svg, page2.svg…
    Folder { parent: PathBuf, name: String },
}

/// Ask where the SVG export of a `pages`-page document goes.
fn svg_destination(title: &str, pages: usize) -> Option<SvgOut> {
    if pages == 1 {
        return save_dialog(title, "svg").map(SvgOut::File);
    }
    rfd::FileDialog::new()
        .set_title(format!(
            "Choose where to save the folder of {pages} SVG pages"
        ))
        .pick_folder()
        .map(|parent| SvgOut::Folder {
            parent,
            name: filename(title),
        })
}

/// Make a new folder `name` in `parent`, as "name (2)" and so on when the
/// name is taken: exports never write into or over existing files.
fn new_folder(parent: &Path, name: &str) -> Result<PathBuf> {
    for attempt in 1..=100 {
        let path = if attempt == 1 {
            parent.join(name)
        } else {
            parent.join(format!("{name} ({attempt})"))
        };
        match std::fs::create_dir(&path) {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error).with_context(|| format!("Could not create {}", path.display()))
            }
        }
    }
    Err(anyhow!(
        "Could not find a free folder name in {}",
        parent.display()
    ))
}

/// The SVG file name of page `index`, counted from one.
fn page_svg_name(index: usize) -> String {
    format!("page{}.svg", index + 1)
}

/// Download every page an export needs, saying how far it has got.
fn download_pages(opened: &mut OpenedDocument, sender: &Sender<Event>) -> Result<()> {
    let total = opened.document.pages.len();
    let mut reported = Instant::now();
    for page in 0..total {
        if total > 1 && reported.elapsed() >= Duration::from_millis(250) {
            let _ = sender.send(Event::Message(format!(
                "Downloading page {} of {total}…",
                page + 1
            )));
            reported = Instant::now();
        }
        opened
            .ensure_page(page)
            .with_context(|| format!("Could not download page {} for export", page + 1))?;
    }
    Ok(())
}

fn export_preview_pdf(
    opened: &mut OpenedDocument,
    out: &Path,
    sender: &Sender<Event>,
) -> Result<()> {
    ensure!(
        opened.document.pages.len() <= 200,
        "Preview PDF export is limited to 200 pages; use the native bundle for larger notebooks"
    );
    download_pages(opened, sender)?;
    let _ = sender.send(Event::Message("Making the PDF…".into()));
    opened.document.export_pdf(out)?;
    // Export advice belongs here, once, rather than in every page's
    // rendering notes.
    let _ = sender.send(Event::Message(format!(
        "Saved to {} — the preview PDF is rasterized; use Original or Native export when fidelity matters.",
        out.display()
    )));
    Ok(())
}

fn export_svg_pages(
    opened: &mut OpenedDocument,
    out: SvgOut,
    sender: &Sender<Event>,
) -> Result<()> {
    download_pages(opened, sender)?;
    match out {
        SvgOut::File(out) => {
            opened.document.export_svg(0, &out)?;
            message_path(sender, &out);
        }
        SvgOut::Folder { parent, name } => {
            let folder = new_folder(&parent, &name)?;
            let total = opened.document.pages.len();
            for page in 0..total {
                let _ = sender.send(Event::Message(format!(
                    "Saving page {} of {total}…",
                    page + 1
                )));
                opened
                    .document
                    .export_svg(page, &folder.join(page_svg_name(page)))
                    .with_context(|| format!("Could not save page {} as SVG", page + 1))?;
            }
            let _ = sender.send(Event::Message(format!(
                "Saved {total} pages to {}",
                folder.display()
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn svg_pages_go_into_a_new_folder_named_after_the_document() {
        let dir = tempfile::tempdir().unwrap();
        let first = new_folder(dir.path(), "Notes").unwrap();
        assert_eq!(first, dir.path().join("Notes"));
        // An earlier export is never written into.
        assert_eq!(
            new_folder(dir.path(), "Notes").unwrap(),
            dir.path().join("Notes (2)")
        );
        assert_eq!(
            new_folder(dir.path(), "Notes").unwrap(),
            dir.path().join("Notes (3)")
        );
        assert_eq!(page_svg_name(0), "page1.svg");
        assert_eq!(page_svg_name(11), "page12.svg");
    }

    /// The full-library download fetches what the viewer reads, nothing else.
    #[test]
    fn the_offline_download_keeps_only_what_the_viewer_reads() {
        let id = "11111111-1111-4111-8111-111111111111";
        let limit = |path: &str| offline_limit(id, &path.replace("ID", id));
        assert_eq!(limit("ID.pdf"), Some(PDF_LIMIT));
        assert_eq!(limit("ID.pagedata"), Some(PAGEDATA_LIMIT));
        assert_eq!(limit("ID/page.rm"), Some(SCENE_LIMIT));
        assert_eq!(
            limit("ID/page/image.png"),
            Some(turborm_core::notebook::MAX_IMAGE_BYTES)
        );
        for path in [
            "ID.epub",
            "ID.content",
            "ID.metadata",
            "ID.thumbnails/page.png",
            "ID/page.json",
            "ID/a/b/c.png",
            "other.pdf",
        ] {
            assert_eq!(limit(path), None, "{path}");
        }
    }

    #[test]
    fn sizes_read_in_binary_units() {
        assert_eq!(format_bytes(12 * 1024), "12 KB");
        assert_eq!(format_bytes(450 * 1024 * 1024), "450 MB");
        assert_eq!(
            format_bytes(2 * 1024 * 1024 * 1024 + 300 * 1024 * 1024),
            "2.3 GB"
        );
    }
    fn page_downloads() -> (
        tempfile::TempDir,
        PageDownloads,
        Vec<turborm_core::notebook::PageRef>,
    ) {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir(directory.path().join("doc")).unwrap();
        let pages: Vec<_> = ["first", "second", "third"]
            .into_iter()
            .map(|id| turborm_core::notebook::PageRef {
                id: id.into(),
                background: None,
            })
            .collect();
        let files = PageDownloads {
            bundle: directory.path().to_owned(),
            id: "doc".into(),
            available: pages
                .iter()
                .map(|page| format!("doc/{}.rm", page.id))
                .collect(),
            bytes: 0,
        };
        (directory, files, pages)
    }

    #[test]
    fn page_downloads_are_lazy_reused_and_complete_for_export() {
        let (_directory, mut files, pages) = page_downloads();
        let mut requested = Vec::new();
        let mut fetch = |name: &str, _limit| {
            requested.push(name.to_owned());
            Ok(vec![1, 2, 3])
        };
        files.page(&pages, 0, &mut fetch).unwrap();
        files.page(&pages, 0, &mut fetch).unwrap();
        assert!(!files.bundle.join("doc/second.rm").exists());
        assert!(files.page(&pages, 3, &mut fetch).is_err());
        for page in 0..pages.len() {
            files.page(&pages, page, &mut fetch).unwrap();
        }
        assert_eq!(requested, ["doc/first.rm", "doc/second.rm", "doc/third.rm"]);
        assert_eq!(files.bytes, 9);
    }

    #[test]
    fn failed_or_oversized_lazy_download_does_not_poison_retry() {
        let (_directory, mut files, pages) = page_downloads();
        assert!(files
            .page(&pages, 0, &mut |_, _| Err(anyhow!("network failure")))
            .is_err());
        assert!(!files.bundle.join("doc/first.rm").exists());
        files.bytes = 512 * 1024 * 1024 - 2;
        assert!(files
            .page(&pages, 0, &mut |_, limit| {
                assert_eq!(limit, 2);
                Ok(vec![0; 3])
            })
            .is_err());
        assert!(!files.bundle.join("doc/first.rm").exists());
        files.page(&pages, 0, &mut |_, _| Ok(vec![0; 2])).unwrap();
        assert!(files.bundle.join("doc/first.rm").exists());
        assert!(files
            .page(&pages, 1, &mut |_, _| panic!(
                "budget must fail before another request"
            ))
            .is_err());
    }

    /// A scene whose image table names `name` and whose one image item uses
    /// it, the minimum `notebook::image_files` needs.
    fn scene_naming_image(name: &str) -> Vec<u8> {
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
        let key = [3u8; 16];
        let mut string = vec![name.len() as u8, 1];
        string.extend(name.as_bytes());
        let mut register = field(0x1f, &[0, 20]);
        register.extend(field(0x2c, &string));
        let mut entry = key.to_vec();
        entry.extend(field(0x1c, &register));
        let mut table = vec![1u8];
        table.extend(field(0x0c, &entry));
        let mut vertices = vec![16u8];
        for (x, y, u, v) in [
            (0.0f32, 0.0f32, 0.0f32, 0.0f32),
            (10.0, 0.0, 1.0, 0.0),
            (10.0, 10.0, 1.0, 1.0),
            (0.0, 10.0, 0.0, 1.0),
        ] {
            for value in [x, y, u, v] {
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
        image.extend(field(0x3c, &vertices));
        image.extend(field(0x4c, &indices));
        let mut envelope = field(0x1f, &[0, 1]);
        envelope.extend(field(0x2f, &[1, 9]));
        envelope.extend(field(0x6c, &image));
        let mut bytes = b"reMarkable .lines file, version=6          ".to_vec();
        bytes.extend(block([0, 3, 3, 14], &field(0x1c, &table)));
        bytes.extend(block([0, 2, 2, 15], &envelope));
        bytes
    }

    #[test]
    fn a_pages_inserted_images_are_fetched_lazily_with_it_and_bounded() {
        let (_directory, mut files, pages) = page_downloads();
        files.available.insert("doc/first/img.png".into());
        files.available.insert("doc/first/unused.png".into());
        let scene = scene_naming_image("img.png");
        let mut requested = Vec::new();
        files
            .page(&pages, 0, &mut |name: &str, limit| {
                requested.push((name.to_owned(), limit));
                Ok(if name.ends_with(".rm") {
                    scene.clone()
                } else {
                    vec![9; 5]
                })
            })
            .unwrap();
        assert_eq!(
            requested,
            [
                ("doc/first.rm".to_owned(), 64 * 1024 * 1024),
                (
                    "doc/first/img.png".to_owned(),
                    turborm_core::notebook::MAX_IMAGE_BYTES
                ),
            ]
        );
        assert_eq!(
            std::fs::read(files.bundle.join("doc/first/img.png")).unwrap(),
            [9; 5]
        );
        // Present files are reused, and other pages fetch nothing extra.
        files
            .page(&pages, 0, &mut |_, _| panic!("already downloaded"))
            .unwrap();
    }

    #[test]
    fn a_failed_image_download_leaves_the_page_usable_and_is_retried() {
        let (_directory, mut files, pages) = page_downloads();
        files.available.insert("doc/first/img.png".into());
        let scene = scene_naming_image("img.png");
        files
            .page(&pages, 0, &mut |name: &str, _| {
                if name.ends_with(".rm") {
                    Ok(scene.clone())
                } else {
                    Err(anyhow!("network failure"))
                }
            })
            .unwrap();
        assert!(files.bundle.join("doc/first.rm").exists());
        assert!(!files.bundle.join("doc/first/img.png").exists());
        files.page(&pages, 0, &mut |_, _| Ok(vec![1])).unwrap();
        assert!(files.bundle.join("doc/first/img.png").exists());
    }

    /// Several tile workers really do render at once.
    ///
    /// The shared receiver is locked only while a worker waits for its next
    /// key, so a slow render never blocks the others from picking work up.
    #[test]
    fn the_tile_pool_renders_several_tiles_concurrently() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let (tx, rx) = mpsc::channel::<usize>();
        let rx = Arc::new(Mutex::new(rx));
        let running = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let (done_tx, done_rx) = mpsc::channel();
        let workers = 4;
        for _ in 0..workers {
            let rx = rx.clone();
            let running = running.clone();
            let peak = peak.clone();
            let done_tx = done_tx.clone();
            std::thread::spawn(move || loop {
                let Some(job) = ({
                    let Ok(rx) = rx.lock() else { return };
                    rx.recv().ok()
                }) else {
                    return;
                };
                let now = running.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(now, Ordering::SeqCst);
                // Stand in for a tile render that takes real time.
                std::thread::sleep(std::time::Duration::from_millis(60));
                running.fetch_sub(1, Ordering::SeqCst);
                if done_tx.send(job).is_err() {
                    return;
                }
            });
        }
        drop(done_tx);
        for job in 0..workers * 2 {
            tx.send(job).unwrap();
        }
        let mut finished = 0;
        while finished < workers * 2 {
            done_rx
                .recv_timeout(std::time::Duration::from_secs(10))
                .expect("the pool must drain");
            finished += 1;
        }
        assert!(
            peak.load(Ordering::SeqCst) > 1,
            "the pool must overlap renders, peaked at {}",
            peak.load(Ordering::SeqCst)
        );
    }

    /// Read-ahead is exactly one page and never more.
    #[test]
    fn prefetch_is_limited_to_the_single_next_page() {
        let document = notebook_document(5);

        // From page 0, the only page worth reading ahead is page 1.
        assert_eq!(next_prefetch(&document, 0), Some(1));
        document.page(1).unwrap();
        // Once it is rendered there is nothing further to prefetch: the read
        // ahead never runs on to page 2.
        assert_eq!(next_prefetch(&document, 0), None);

        // Moving on re-arms it for exactly one page again.
        assert_eq!(next_prefetch(&document, 1), Some(2));
        assert_eq!(next_prefetch(&document, 2), Some(3));

        // The last page has no successor to read ahead.
        assert_eq!(next_prefetch(&document, 4), None);
        assert_eq!(next_prefetch(&document, usize::MAX), None);
    }

    /// Pages already rendered are served from the cache, so stepping back is
    /// immediate and asks the cloud for nothing.
    #[test]
    fn the_page_cache_makes_stepping_back_a_cache_hit() {
        let document = notebook_document(6);
        for index in 0..4 {
            document.page(index).unwrap();
        }
        // The last four pages visited are all immediate.
        for index in 0..4 {
            assert!(document.is_page_cached(index), "page {index}");
        }
        // A fifth page evicts the least recently used one, not a recent one.
        document.page(4).unwrap();
        assert!(!document.is_page_cached(0));
        for index in 1..=4 {
            assert!(document.is_page_cached(index), "page {index}");
        }
        // Re-visiting a cached page makes it the most recent again, so the
        // next eviction takes a different page.
        document.page(1).unwrap();
        document.page(5).unwrap();
        assert!(document.is_page_cached(1), "the revisited page survives");
        assert!(!document.is_page_cached(2));
    }

    /// A local notebook with `pages` blank pages, for cache tests.
    fn notebook_document(pages: usize) -> Document {
        let directory = tempfile::tempdir().unwrap();
        let id = "00000000-0000-0000-0000-000000000050".to_owned();
        let bundle = directory.path().join("bundle");
        let page_ids: Vec<String> = (0..pages)
            .map(|index| format!("00000000-0000-0000-0000-0000000000{index:02}"))
            .collect();
        std::fs::create_dir_all(bundle.join(&id)).unwrap();
        std::fs::write(
            bundle.join(format!("{id}.content")),
            serde_json::json!({"formatVersion":1,"fileType":"notebook","pages":page_ids})
                .to_string(),
        )
        .unwrap();
        Document::load(directory, id, "Notes".into()).unwrap()
    }

    #[test]
    fn remote_titles_cannot_be_export_paths() {
        assert_eq!(filename("../../secret\n"), "secret");
        assert_eq!(filename(".."), "document");
        assert!(!filename("folder\\name/").contains(['/', '\\']));
    }
}
