use crate::{
    library_state::{LibraryState, Scope, Sort, View},
    slint_generated::{AppWindow, DragState, FolderTarget, LibraryItem, LibraryRow, ViewerTile},
    viewer_state::{Fit, ViewerState},
};
use anyhow::{Context, Result};
use slint::{
    ComponentHandle, Image, Model, ModelRc, Rgba8Pixel, SharedPixelBuffer, SharedString, VecModel,
};
use std::{
    cell::RefCell,
    collections::HashMap,
    path::PathBuf,
    process::{Command as Process, Stdio},
    rc::Rc,
    sync::mpsc::{Receiver, Sender},
    time::Duration,
};
use turborm::{
    renderer::{Preview, Tile},
    service::{self, format_bytes, Command, Event, ExportFormat, Offline},
    settings::{CacheMode, Settings, SortOrder},
};
use turborm_core::library::Entry;
use zeroize::Zeroizing;

// Folder tiles are as wide as their names within these bounds, with this gap
// between them. Must match `folder-strip.slint`.
const FOLDER_MIN: f32 = 112.0;
const FOLDER_MAX: f32 = 320.0;
const FOLDER_GAP: f32 = 10.0;
// Card width (170) plus the row spacing (14).
const COVER_CELL: f32 = 184.0;
const CONNECT_URL: &str = "https://my.remarkable.com/device/browser/connect";

struct Controller {
    state: LibraryState,
    viewer: ViewerState,
    /// The viewer replaces the library in the same window; library state is
    /// untouched while it is open so going back restores selection and folder.
    viewer_screen: bool,
    tiles: Rc<VecModel<ViewerTile>>,
    tile_ids: Vec<String>,
    /// The document being opened, so its library cover can stand in for the
    /// page until the first raster arrives.
    opening_id: Option<String>,
    tx: Sender<Command>,
    rx: Receiver<Event>,
    viewport_width: f32,
    entries: Rc<VecModel<LibraryItem>>,
    folder_rows: Rc<VecModel<LibraryRow>>,
    cover_rows: Rc<VecModel<LibraryRow>>,
    breadcrumbs: Rc<VecModel<FolderTarget>>,
    move_targets: Rc<VecModel<FolderTarget>>,
    service_in_flight: bool,
    /// The pairing screen stays up until the service reports a paired device.
    pairing_screen: bool,
    cover_positions: HashMap<String, (usize, usize)>,
    cover_row_models: Vec<Rc<VecModel<LibraryItem>>>,
    list_positions: HashMap<String, usize>,
    folder_positions: HashMap<String, (usize, usize)>,
    folder_row_models: Vec<Rc<VecModel<LibraryItem>>>,
    /// Each visible folder tile's natural width, in order, so a resize can
    /// tell whether the rows would break differently.
    folder_widths: Vec<f32>,
    settings: Settings,
    /// Only the real app writes the settings file; demos and tests do not.
    persist_settings: bool,
    settings_visible: bool,
    /// Disk used by downloaded documents, once measured.
    storage_used: Option<u64>,
    /// What the whole library would take, once known.
    library_bytes: Option<u64>,
    offline: Offline,
}

impl Controller {
    fn new(state: LibraryState, tx: Sender<Command>, rx: Receiver<Event>) -> Self {
        let demo = state.demo;
        Self {
            state,
            viewer: ViewerState::new(demo),
            viewer_screen: false,
            tiles: Rc::new(VecModel::default()),
            tile_ids: Vec::new(),
            opening_id: None,
            tx,
            rx,
            viewport_width: 900.0,
            entries: Rc::new(VecModel::default()),
            folder_rows: Rc::new(VecModel::default()),
            cover_rows: Rc::new(VecModel::default()),
            breadcrumbs: Rc::new(VecModel::default()),
            move_targets: Rc::new(VecModel::default()),
            service_in_flight: false,
            pairing_screen: false,
            cover_positions: HashMap::new(),
            cover_row_models: Vec::new(),
            list_positions: HashMap::new(),
            folder_positions: HashMap::new(),
            folder_row_models: Vec::new(),
            folder_widths: Vec::new(),
            settings: Settings::default(),
            persist_settings: false,
            settings_visible: false,
            storage_used: None,
            library_bytes: None,
            offline: Offline::Idle,
        }
    }

    /// Pushes the Settings dialog's values. Cheap: no model is rebuilt.
    fn project_settings(&self, window: &AppWindow) {
        let full = self.settings.cache == CacheMode::Full;
        window.set_settings_visible(self.settings_visible);
        window.set_cache_mode(self.settings.cache.as_str().into());
        window.set_storage_text(
            match (self.state.demo, self.storage_used) {
                (true, _) => "Storage is not used in demo mode".to_owned(),
                (false, None) => "Measuring…".to_owned(),
                (false, Some(0)) => "No documents are downloaded".to_owned(),
                (false, Some(bytes)) => format!("Downloaded documents use {}", format_bytes(bytes)),
            }
            .into(),
        );
        window.set_library_size_text(
            self.library_bytes
                .map_or_else(String::new, |bytes| {
                    format!("about {}", format_bytes(bytes))
                })
                .into(),
        );
        let (long, short) = offline_text(full, &self.offline);
        window.set_offline_text(long.into());
        window.set_offline_summary(short.into());
        window.set_offline_warning(full && matches!(self.offline, Offline::Stopped(_)));
        window.set_can_clear_cache(
            !self.state.demo && !full && self.storage_used.is_some_and(|bytes| bytes > 0),
        );
    }

    /// Settings commands skip [`Controller::send`]: they run beside whatever
    /// the foreground is doing and never make the library look busy.
    fn send_setting(&mut self, command: Command) {
        if self.state.demo {
            return;
        }
        if self.tx.send(command).is_err() {
            self.state.error = "The background service stopped. Restart the app.".into();
        }
    }

    fn save_settings(&mut self) {
        if self.persist_settings {
            if let Err(error) = self.settings.save() {
                self.state.error = format!("Could not save the settings: {error:#}");
            }
        }
    }

    /// Orders the library and remembers the order for the next launch.
    fn set_sort(&mut self, sort: Sort) {
        self.state.sort = sort;
        let order = match sort {
            Sort::Modified => SortOrder::Modified,
            Sort::Name => SortOrder::Name,
        };
        if order != self.settings.sort {
            self.settings.sort = order;
            self.save_settings();
        }
    }

    fn set_cache_mode(&mut self, mode: CacheMode) {
        if mode == self.settings.cache {
            return;
        }
        self.settings.cache = mode;
        self.save_settings();
        self.offline = if mode == CacheMode::Full && !self.state.demo {
            // Until the first report, say that the pass is being prepared.
            Offline::Saving { done: 0, total: 0 }
        } else {
            Offline::Idle
        };
        self.send_setting(Command::SetCacheMode(mode));
    }

    fn item(&self, entry: &Entry) -> LibraryItem {
        let cover = self.state.cover(&entry.id).cloned().unwrap_or_default();
        LibraryItem {
            entry_id: entry.id.clone().into(),
            title: entry.title.clone().into(),
            subtitle: subtitle(entry).into(),
            is_folder: entry.kind == "folder",
            has_content: entry.kind == "folder" && self.state.has_content(&entry.id),
            pinned: entry.pinned,
            selected: self.state.selected.as_deref() == Some(&entry.id),
            cover,
            cover_ready: self.state.cover(&entry.id).is_some(),
            warning: if self.state.cover_failed(&entry.id) {
                "Preview unavailable".into()
            } else {
                entry.hierarchy_warning.clone().unwrap_or_default().into()
            },
            type_label: type_label(entry).into(),
            tile_width: if entry.kind == "folder" {
                folder_tile_width(&entry.title, entry.pinned)
                    .min(folder_row_width(self.viewport_width))
            } else {
                0.0
            },
        }
    }

    fn replace_model<T: Clone + 'static>(
        model: &Rc<VecModel<T>>,
        values: impl IntoIterator<Item = T>,
    ) {
        model.set_vec(values.into_iter().collect::<Vec<_>>());
    }

    fn project(&mut self, window: &AppWindow) {
        self.state.clear_queued_covers();
        let visible = self.state.visible();
        let folders = visible
            .iter()
            .copied()
            .filter(|&i| self.state.entries[i].kind == "folder")
            .collect::<Vec<_>>();
        let documents = visible
            .iter()
            .copied()
            .filter(|&i| self.state.entries[i].kind == "document")
            .collect::<Vec<_>>();
        self.list_positions.clear();
        if self.state.view == View::List {
            for (row, &index) in visible.iter().enumerate() {
                self.list_positions
                    .insert(self.state.entries[index].id.clone(), row);
            }
        }
        Self::replace_model(
            &self.entries,
            (self.state.view == View::List)
                .then_some(visible.iter())
                .into_iter()
                .flatten()
                .map(|&i| self.item(&self.state.entries[i])),
        );
        let usable_width = folder_row_width(self.viewport_width);
        // Tiles fit their names, so each row takes as many as its width holds.
        self.folder_widths = folders
            .iter()
            .map(|&i| {
                let entry = &self.state.entries[i];
                folder_tile_width(&entry.title, entry.pinned)
            })
            .collect();
        self.folder_positions.clear();
        self.folder_row_models.clear();
        let mut remaining = folders.iter();
        let folder_rows = pack_rows(&self.folder_widths, usable_width)
            .into_iter()
            .enumerate()
            .map(|(row, count)| {
                let items = remaining
                    .by_ref()
                    .take(count)
                    .map(|&i| self.item(&self.state.entries[i]))
                    .collect::<Vec<_>>();
                for (column, item) in items.iter().enumerate() {
                    self.folder_positions
                        .insert(item.entry_id.to_string(), (row, column));
                }
                let model = Rc::new(VecModel::from(items));
                self.folder_row_models.push(model.clone());
                LibraryRow {
                    items: ModelRc::from(model),
                }
            })
            .collect::<Vec<_>>();
        Self::replace_model(&self.folder_rows, folder_rows);
        let cover_columns = ((usable_width / COVER_CELL).floor() as usize).max(1);
        self.cover_positions.clear();
        self.cover_row_models.clear();
        let cover_rows = (self.state.view == View::Covers)
            .then_some(documents.chunks(cover_columns))
            .into_iter()
            .flatten()
            .enumerate()
            .map(|(row, chunk)| {
                for (column, &index) in chunk.iter().enumerate() {
                    self.cover_positions
                        .insert(self.state.entries[index].id.clone(), (row, column));
                }
                let model = Rc::new(VecModel::from(
                    chunk
                        .iter()
                        .map(|&i| self.item(&self.state.entries[i]))
                        .collect::<Vec<_>>(),
                ));
                self.cover_row_models.push(model.clone());
                LibraryRow {
                    items: ModelRc::from(model),
                }
            })
            .collect::<Vec<_>>();
        Self::replace_model(&self.cover_rows, cover_rows);
        // Slint stretches cards across the row and needs the same column count
        // Rust used to chunk them, so a short last row stays left-aligned.
        window.set_cover_columns(i32::try_from(cover_columns).unwrap_or(i32::MAX));
        Self::replace_model(
            &self.breadcrumbs,
            self.state
                .breadcrumbs()
                .into_iter()
                .map(|(id, title)| FolderTarget {
                    entry_id: id.into(),
                    title: title.into(),
                }),
        );
        Self::replace_model(
            &self.move_targets,
            self.state
                .move_visible
                .then(|| self.state.move_targets())
                .into_iter()
                .flatten()
                .map(|(id, title)| FolderTarget {
                    entry_id: id.into(),
                    title: title.into(),
                }),
        );
        let selected = self
            .state
            .selected
            .as_deref()
            .and_then(|id| self.state.entry(id));
        let selected_doc = selected.is_some_and(|e| e.kind == "document");
        let heading = if self.state.scope != Scope::Folder || self.state.folder.is_empty() {
            self.state.scope.heading().to_string()
        } else {
            self.state
                .entry(&self.state.folder)
                .map_or_else(|| Scope::Folder.heading().to_string(), |e| e.title.clone())
        };
        let document_count = documents.len();
        let folder_count = folders.len();
        let subtitle = format!(
            "{document_count} document{} · {folder_count} folder{}",
            if document_count == 1 { "" } else { "s" },
            if folder_count == 1 { "" } else { "s" },
        );
        window.set_screen(
            if self.pairing_screen {
                "pairing"
            } else if self.viewer_screen {
                "viewer"
            } else {
                "library"
            }
            .into(),
        );
        window.set_title_text(format!("TurboRM · {heading}").into());
        window.set_heading_text(heading.into());
        window.set_heading_subtitle(subtitle.into());
        window.set_search_text(self.state.search.clone().into());
        window.set_error_text(self.state.error.clone().into());
        window.set_status_text(self.state.status.clone().into());
        window.set_selection_title(
            selected
                .map_or_else(String::new, |e| e.title.clone())
                .into(),
        );
        window.set_busy(self.state.busy);
        window.set_paired(self.state.paired.unwrap_or(false));
        window.set_pairing_checking(self.state.paired.is_none() && !self.state.demo);
        window.set_demo_mode(self.state.demo);
        window.set_filter(self.state.scope.as_str().into());
        window.set_sort(self.state.sort.as_str().into());
        window.set_covers_view(self.state.view == View::Covers);
        window.set_can_go_back(self.state.scope == Scope::Folder && !self.state.folder.is_empty());
        window.set_can_open(selected_doc);
        window.set_can_move(selected.is_some() && !self.state.demo);
        window.set_can_export_original(selected_doc && !self.state.demo);
        window.set_can_export_native(selected_doc && !self.state.demo);
        window.set_can_export_pages(selected_doc && !self.state.demo);
        window.set_can_import(
            self.state.paired == Some(true)
                && self.state.scope == Scope::Folder
                && !self.state.demo,
        );
        window.set_can_verify_import(self.state.paired == Some(true) && !self.state.demo);
        window.set_move_visible(self.state.move_visible);
        window.set_can_create_folder(self.state.can_create_folder());
        window.set_can_create_inside(self.state.can_create_inside());
        window
            .global::<DragState>()
            .set_enabled(self.state.can_drag());
        let dialog = self.state.folder_dialog.clone().unwrap_or_default();
        window.set_folder_dialog_visible(self.state.folder_dialog.is_some());
        window.set_folder_dialog_name(dialog.name.into());
        window.set_folder_dialog_error(dialog.error.into());
        window.set_folder_dialog_location(self.state.folder_dialog_location().into());
        self.project_settings(window);
        self.project_viewer(window);
    }

    /// Pushes the viewer's Rust-owned state onto the generated properties.
    fn project_viewer(&mut self, window: &AppWindow) {
        window.set_document_title(self.viewer.title().into());
        window.set_viewer_has_document(self.viewer.has_document());
        window.set_viewer_page(i32::try_from(self.viewer.page()).unwrap_or(i32::MAX));
        window.set_viewer_total(i32::try_from(self.viewer.total()).unwrap_or(i32::MAX));
        window.set_viewer_page_field(self.viewer.page_field.clone().into());
        window.set_viewer_fit_mode(self.viewer.fit.as_str().into());
        window.set_viewer_zoom_percent(self.viewer.zoom_percent());
        window.set_viewer_can_export(self.viewer.preview.is_some() && !self.state.demo);
        window.set_viewer_warnings(ModelRc::from(
            self.viewer
                .warnings()
                .into_iter()
                .map(SharedString::from)
                .collect::<Vec<_>>()
                .as_slice(),
        ));
        window.set_viewer_notes(ModelRc::from(
            self.viewer
                .notes()
                .into_iter()
                .map(SharedString::from)
                .collect::<Vec<_>>()
                .as_slice(),
        ));
        let (page_width, page_height) = self.viewer.page_size();
        window.set_viewer_page_width(page_width);
        window.set_viewer_page_height(page_height);
        // Keep painting the previous page (or the library cover) until the
        // replacement raster lands, so the canvas never goes blank.
        window.set_viewer_overview(self.viewer.visible_overview().cloned().unwrap_or_default());
        window.set_viewer_overview_ready(self.viewer.visible_overview().is_some());
        window.set_viewer_stale(self.viewer.showing_stale_page());
        self.sync_tiles(window);
        // While a page is being prepared, the status line names it, and while
        // tiles are still arriving it says how many are outstanding.
        if let Some(page) = self.viewer.pending_page() {
            if self.state.status.is_empty() {
                self.state.status = format!("Loading page {}…", page + 1);
            }
        } else if self.viewer.in_flight_tiles() > 0 && self.state.status.is_empty() {
            self.state.status = format!("Sharpening {} tiles…", self.viewer.in_flight_tiles());
        }
        window.set_status_text(self.state.status.clone().into());
    }

    /// Updates the tile rows in place: rows keep their identity while the
    /// visible set is unchanged, so Slint reuses the existing image elements.
    fn sync_tiles(&mut self, window: &AppWindow) {
        // The raster density follows the zoom, the fit and the display. It is
        // applied here, where tiles are placed and requested, so a page that
        // has just arrived sharpens at once. Applying it only on a scroll
        // left a new page at its blurry first paint until the user scrolled.
        let scale = self
            .viewer
            .raster_scale_milli(window.window().scale_factor());
        self.viewer.sync_scale(scale);
        let placed = self.viewer.placed_tiles();
        for (index, (tile, image)) in placed.iter().enumerate() {
            let row = ViewerTile {
                tile_id: tile.id.clone().into(),
                x: tile.x,
                y: tile.y,
                w: tile.width,
                h: tile.height,
                image: image.clone(),
            };
            match self.tile_ids.get(index) {
                Some(existing) if existing == &tile.id => {
                    // Geometry still changes on zoom, so refresh the row.
                    self.tiles.set_row_data(index, row);
                }
                Some(_) => {
                    self.tiles.set_row_data(index, row);
                    self.tile_ids[index] = tile.id.clone();
                }
                None => {
                    self.tiles.push(row);
                    self.tile_ids.push(tile.id.clone());
                }
            }
        }
        while self.tile_ids.len() > placed.len() {
            self.tiles.remove(self.tile_ids.len() - 1);
            self.tile_ids.pop();
        }
        window.set_viewer_tiles(ModelRc::from(self.tiles.clone()));
        self.request_tiles();
    }

    /// Asks the worker pool for every missing visible tile at once, ordered
    /// centre-out. The pool renders them in parallel.
    fn request_tiles(&mut self) {
        let requests = self.viewer.next_tile_requests();
        if requests.is_empty() {
            return;
        }
        let keys: Vec<_> = requests.iter().map(|(key, _)| key.clone()).collect();
        if self.tx.send(Command::Tiles(keys)).is_err() {
            for (key, generation) in requests {
                self.viewer
                    .finish_tile(key, generation, Err("The renderer stopped.".into()));
            }
        }
    }

    fn finish_tile(&mut self, result: Result<Tile, String>) {
        let generation = self.viewer.generation();
        match result {
            Ok(tile) => {
                let image = raster_image(tile.width, tile.height, &tile.rgba);
                let outcome = image.map_or_else(
                    || Err("Tile raster was malformed".to_string()),
                    |image| Ok((image, tile.width, tile.height)),
                );
                self.viewer.finish_tile(tile.key, generation, outcome);
            }
            Err(error) => {
                // The pool reports which tile failed only through the error
                // text; record it so the viewer can explain, without marking
                // an arbitrary key as failed.
                self.viewer.tile_error = error;
            }
        }
    }

    /// Requests a page, or moves straight there in offline demo mode.
    fn go_page(&mut self, page: usize) {
        let Some(preview) = self.viewer.preview.as_mut() else {
            return;
        };
        if page >= preview.total || page == preview.page {
            self.viewer.page_field = (preview.page + 1).to_string();
            return;
        }
        if self.state.demo {
            preview.page = page;
            self.viewer.page_field = (page + 1).to_string();
            // Demo pages share one synthetic raster, so only the label moves.
        } else {
            let previous = preview.page;
            self.viewer.begin_page(page);
            if self.tx.send(Command::Page(page)).is_err() {
                self.viewer.begin_page(previous);
                self.state.error = "The background service stopped. Restart the app.".into();
                return;
            }
            self.state.error.clear();
            self.state.status = format!("Loading page {}…", page + 1);
            self.service_in_flight = true;
            self.state.busy = true;
        }
    }

    fn update_cover_item(&self, id: &str) {
        let Some(&(row, column)) = self.cover_positions.get(id) else {
            return;
        };
        let Some(entry) = self.state.entry(id) else {
            return;
        };
        if let Some(model) = self.cover_row_models.get(row) {
            model.set_row_data(column, self.item(entry));
        }
    }

    fn update_item(&self, id: &str) {
        let Some(entry) = self.state.entry(id) else {
            return;
        };
        let item = self.item(entry);
        if let Some(&row) = self.list_positions.get(id) {
            self.entries.set_row_data(row, item.clone());
        }
        if let Some(&(row, column)) = self.cover_positions.get(id) {
            if let Some(model) = self.cover_row_models.get(row) {
                model.set_row_data(column, item);
            }
        }
        if let Some(&(row, column)) = self.folder_positions.get(id) {
            if let Some(model) = self.folder_row_models.get(row) {
                model.set_row_data(column, self.item(entry));
            }
        }
    }

    fn send(&mut self, command: Command) {
        if self.state.demo {
            self.state.status = "Cloud actions are unavailable in offline demo mode".into();
            return;
        }
        if self.state.busy {
            return;
        }
        self.state.error.clear();
        self.state.status.clear();
        match self.tx.send(command) {
            Ok(()) => {
                self.service_in_flight = true;
                self.state.busy = true;
            }
            Err(_) => self.state.error = "The background service stopped. Restart the app.".into(),
        }
    }

    fn drain(&mut self, window: &AppWindow) {
        let mut rebuild = false;
        let mut tiles_changed = false;
        let mut settings_changed = false;
        while let Ok(event) = self.rx.try_recv() {
            match event {
                Event::Paired(value) => {
                    self.state.paired = Some(value);
                    // Pairing now has a native screen: show it while unpaired
                    // and return to the library as soon as a token exists.
                    self.pairing_screen = !value && !self.state.demo;
                    if value {
                        // The one-time code is never retained after pairing.
                        window.invoke_clear_pair_code();
                    }
                    rebuild = true;
                }
                Event::Listing(entries) => {
                    self.state.replace_entries(entries);
                    rebuild = true;
                }
                Event::Thumbnail { id, epoch, preview } => {
                    let image = preview.ok().and_then(preview_image).ok_or(());
                    if self.state.finish_cover(id.clone(), epoch, image) {
                        self.update_cover_item(&id);
                        if let Some(evicted) = self.state.take_evicted_cover() {
                            self.update_cover_item(&evicted);
                        }
                        if let Some((next, epoch)) = self.state.next_cover_request() {
                            if self
                                .tx
                                .send(Command::Thumbnail {
                                    id: next.clone(),
                                    epoch,
                                })
                                .is_err()
                            {
                                self.state.finish_cover(next, epoch, Err(()));
                            }
                        }
                    }
                }
                Event::Message(message) => self.state.status = message,
                Event::Error(error) => self.state.error = error,
                Event::Finished => {
                    self.service_in_flight = false;
                    self.state.busy = false;
                }
                Event::FolderCreated(id) => {
                    if self.state.entry(&id).is_some() {
                        self.state.selected = Some(id);
                    }
                    rebuild = true;
                }
                Event::Opening { title, pages, page } => {
                    // Switch the viewer before the download: the library's
                    // cached cover stands in for the page until it renders.
                    let placeholder = self
                        .opening_id
                        .as_deref()
                        .and_then(|id| self.state.cover(id))
                        .cloned();
                    self.viewer.begin_opening(title, pages, page, placeholder);
                    self.viewer_screen = true;
                    rebuild = true;
                }
                Event::Preview(preview) => {
                    let overview = raster_image(preview.width, preview.height, &preview.rgba);
                    self.viewer.set_preview(preview, overview);
                    self.viewer_screen = true;
                    // The first raster is on screen; the progress line has
                    // nothing left to say.
                    self.state.status.clear();
                    rebuild = true;
                }
                Event::Tile(result) => {
                    self.finish_tile(result);
                    tiles_changed = true;
                }
                Event::Storage(bytes) => {
                    self.storage_used = Some(bytes);
                    settings_changed = true;
                }
                Event::LibrarySize(bytes) => {
                    self.library_bytes = Some(bytes);
                    settings_changed = true;
                }
                Event::Offline(progress) => {
                    // A finished pass changed what is on disk.
                    if !matches!(progress, Offline::Saving { .. }) && self.settings_visible {
                        self.send_setting(Command::MeasureStorage);
                    }
                    self.offline = progress;
                    settings_changed = true;
                }
            }
        }
        if rebuild {
            self.project(window);
        } else {
            window.set_error_text(self.state.error.clone().into());
            window.set_status_text(self.state.status.clone().into());
            window.set_busy(self.state.busy);
            if tiles_changed {
                self.sync_tiles(window);
            }
            if settings_changed {
                self.project_settings(window);
            }
        }
    }
}

/// The Settings dialog's progress line and the sidebar's few words for the
/// full-library download.
fn offline_text(full: bool, offline: &Offline) -> (String, String) {
    if !full {
        return (String::new(), String::new());
    }
    match offline {
        Offline::Idle => (
            "The download starts once the library is listed".into(),
            String::new(),
        ),
        Offline::Saving { total: 0, .. } => (
            "Checking which documents to download…".into(),
            "saving offline".into(),
        ),
        Offline::Saving { done, total } => {
            let percent = (done.saturating_mul(100) / total).min(99);
            (
                format!(
                    "Downloading your library: {} of {} ({percent}%)",
                    format_bytes(*done),
                    format_bytes(*total)
                ),
                format!("saving offline {percent}%"),
            )
        }
        Offline::Ready { library } => (
            format!(
                "Every document is saved on this computer ({})",
                format_bytes(*library)
            ),
            "saved offline".into(),
        ),
        Offline::Stopped(reason) => (reason.clone(), "offline paused".into()),
    }
}

fn preview_image(preview: Preview) -> Option<Image> {
    raster_image(preview.width, preview.height, &preview.rgba)
}

/// Copies an RGBA raster into a Slint image, rejecting mismatched buffers.
fn raster_image(width: usize, height: usize, rgba: &[u8]) -> Option<Image> {
    let pixel_width = u32::try_from(width).ok()?;
    let pixel_height = u32::try_from(height).ok()?;
    if rgba.len() != width.checked_mul(height)?.checked_mul(4)? {
        return None;
    }
    let mut pixels = SharedPixelBuffer::<Rgba8Pixel>::new(pixel_width, pixel_height);
    pixels.make_mut_bytes().copy_from_slice(rgba);
    Some(Image::from_rgba8(pixels))
}

/// The width a row of folder tiles may fill in a library `viewport` wide.
fn folder_row_width(viewport: f32) -> f32 {
    (viewport - 14.0).max(FOLDER_MIN)
}

/// How many tiles of these widths each row of `row_width` holds, in order.
/// A tile wider than the row is cut to fit it and has a row of its own.
fn pack_rows(widths: &[f32], row_width: f32) -> Vec<usize> {
    let mut rows = Vec::new();
    let mut used = 0.0;
    for &width in widths {
        let width = width.min(row_width);
        match rows.last_mut() {
            Some(count) if used + FOLDER_GAP + width <= row_width => {
                *count += 1;
                used += FOLDER_GAP + width;
            }
            _ => {
                rows.push(1);
                used = width;
            }
        }
    }
    rows
}

/// A folder tile's width: the icon, the name in Inter Medium at the body size
/// and the padding, as `folder-strip.slint` lays them out.
///
/// Characters Inter lacks are counted a full em wide, which is about what the
/// system's fallback fonts draw.
fn folder_tile_width(title: &str, pinned: bool) -> f32 {
    static FACE: std::sync::LazyLock<Option<ttf_parser::Face<'static>>> =
        std::sync::LazyLock::new(|| {
            ttf_parser::Face::parse(include_bytes!("../fonts/Inter-Medium.ttf"), 0).ok()
        });
    const FONT_SIZE: f32 = 13.0;
    let ems: f32 = title
        .chars()
        .map(|c| {
            FACE.as_ref()
                .and_then(|face| {
                    let advance = face.glyph_hor_advance(face.glyph_index(c)?)?;
                    Some(f32::from(advance) / f32::from(face.units_per_em()))
                })
                .unwrap_or(1.0)
        })
        .sum();
    let text = ems * FONT_SIZE;
    // The name starts 37px in and keeps 10px clear on the right, or 23px
    // beside the pin star; 2px more absorbs rounding.
    let padding = 37.0 + if pinned { 23.0 } else { 10.0 } + 2.0;
    (text.ceil() + padding).clamp(FOLDER_MIN, FOLDER_MAX)
}

fn demo_entries(count: usize, folder_count: usize) -> Vec<Entry> {
    // Synthetic names, types and page counts. No cloud data is touched; the
    // spread exists so the sidebar filters and the sort have something to act
    // on in the offline demos.
    // Short and long names, so folder tiles of every width show.
    const FOLDERS: [&str; 10] = [
        "Books",
        "Articles",
        "Software & hardware",
        "Claude Review",
        "Mechanical engineering lecture notes",
        "To-do",
        "Research",
        "reMarkable",
        "Work",
        "Archive",
    ];
    const TITLES: [&str; 6] = [
        "Friday meeting",
        "Planning",
        "Vision 2025",
        "Field notes",
        "Reading list",
        "Sketches",
    ];
    const TYPES: [&str; 3] = ["notebook", "pdf", "epub"];
    (0..count)
        .map(|i| {
            let folder = i < folder_count;
            let index = i - if folder { 0 } else { folder_count };
            Entry {
                id: format!("demo-{i}"),
                title: if folder {
                    FOLDERS.get(index).map_or_else(
                        || format!("Folder {}", index + 1),
                        |name| (*name).to_owned(),
                    )
                } else if index < TITLES.len() {
                    TITLES[index].to_owned()
                } else {
                    format!(
                        "{} {}",
                        TITLES[index % TITLES.len()],
                        index / TITLES.len() + 1
                    )
                },
                kind: if folder { "folder" } else { "document" }.into(),
                // The last document lives in the first folder, so the demo
                // shows a filled folder next to empty ones.
                parent: if !folder && folder_count > 0 && i + 1 == count {
                    "demo-0".into()
                } else {
                    String::new()
                },
                deleted: false,
                pinned: i % 7 == 0,
                // Descending so the default "recently modified" order matches
                // the order the entries are generated in.
                modified: Some(format!(
                    "{}",
                    1_790_000_000_000u64 - (i as u64) * 86_400_000
                )),
                files: 0,
                size: 0,
                path: String::new(),
                hierarchy_warning: None,
                file_type: (!folder).then(|| TYPES[index % TYPES.len()].to_owned()),
                page_count: (!folder).then(|| u32::try_from(index % 23 + 1).unwrap_or(1)),
            }
        })
        .collect()
}

pub fn run() -> Result<()> {
    let args = std::env::args().collect::<Vec<_>>();
    let stress = args.iter().any(|a| a == "--demo-stress");
    let demo_pairing = args.iter().any(|a| a == "--demo-pairing");
    let demo_viewer = args.iter().any(|a| a == "--demo-viewer");
    let demo_long = args.iter().any(|a| a == "--demo-long");
    let demo_landscape = args.iter().any(|a| a == "--demo-landscape");
    // Opens the offline demo with the New folder dialog up, for screenshots.
    let demo_new_folder = args.iter().any(|a| a == "--demo-new-folder");
    // Opens the offline demo with the Settings dialog up, for screenshots.
    let demo_settings = args.iter().any(|a| a == "--demo-settings");
    // Opens the offline demo with ten folders of different name lengths.
    let demo_folders = args.iter().any(|a| a == "--demo-folders");
    // Opens the offline demo mid-drag, with the label over a folder.
    let demo_drag = args.iter().any(|a| a == "--demo-drag");
    let demo = stress
        || demo_folders
        || demo_drag
        || demo_new_folder
        || demo_settings
        || demo_pairing
        || demo_viewer
        || demo_long
        || demo_landscape
        || args.iter().any(|a| a == "--demo");
    let demo_list = args.iter().any(|a| a == "--demo-list");
    let demo_dark = args.iter().any(|a| a == "--demo-dark");
    // Opens the offline demo with an entry selected and the context menu up,
    // so the toolbar's selected state and the menu can be reviewed offline.
    let demo_selected = args.iter().any(|a| a == "--demo-selected");
    let open_document = args
        .windows(2)
        .find(|p| p[0] == "--open-document")
        .map(|p| p[1].clone());
    let screenshot = args
        .windows(2)
        .find(|p| p[0] == "--screenshot")
        .map(|p| p[1].clone());
    if screenshot.is_some() && !demo {
        anyhow::bail!("--screenshot requires one of the --demo modes");
    }
    let window = AppWindow::new().context("could not create the Slint library window")?;
    // Matches turborm.desktop, so Wayland docks show the TurboRM icon for the
    // window. It needs the platform AppWindow::new starts, and must precede show.
    slint::set_xdg_app_id("turborm").context("could not set the window app ID")?;
    window
        .window()
        .set_size(slint::LogicalSize::new(1080.0, 760.0));
    if demo && args.iter().any(|arg| arg == "--demo-narrow") {
        window
            .window()
            .set_size(slint::LogicalSize::new(760.0, 520.0));
    }
    let weak = window.as_weak();
    // Demos never read or write the real settings file.
    let settings = if demo {
        Settings::default()
    } else {
        Settings::load()
    };
    let (tx, rx) = service::start(settings.cache, move || {
        let wake = weak.clone();
        let _ = wake.upgrade_in_event_loop(|window| window.invoke_worker_wakeup());
    });
    window.set_theme_dark(demo_dark);
    let mut state = LibraryState::new(demo);
    if demo_list {
        state.view = View::List;
    }
    if demo && !demo_pairing {
        state.paired = Some(true);
        state.status = if stress {
            "Offline stress demo · 50,000 entries"
        } else {
            "Offline sample library"
        }
        .into();
        state.replace_entries(demo_entries(
            if stress {
                50_000
            } else if demo_folders {
                18
            } else {
                10
            },
            if stress {
                70
            } else if demo_folders {
                10
            } else {
                2
            },
        ));
    } else if demo_pairing {
        state.paired = Some(false);
        state.status = "Demo mode · pairing is disabled".into();
    }
    let controller = Rc::new(RefCell::new(Controller::new(state, tx, rx)));
    controller.borrow_mut().pairing_screen = demo_pairing;
    controller.borrow_mut().state.sort = Sort::from_str(settings.sort.as_str());
    controller.borrow_mut().settings = settings;
    controller.borrow_mut().persist_settings = !demo;
    window.set_entries(ModelRc::from(controller.borrow().entries.clone()));
    window.set_folder_rows(ModelRc::from(controller.borrow().folder_rows.clone()));
    window.set_cover_rows(ModelRc::from(controller.borrow().cover_rows.clone()));
    window.set_breadcrumbs(ModelRc::from(controller.borrow().breadcrumbs.clone()));
    window.set_move_targets(ModelRc::from(controller.borrow().move_targets.clone()));
    window.set_viewer_tiles(ModelRc::from(controller.borrow().tiles.clone()));
    install_callbacks(&window, &controller);
    if demo_viewer || demo_long || demo_landscape {
        // The demo viewer opens a synthetic page; no document is downloaded.
        let title = if demo_long {
            "Research paper"
        } else if demo_landscape {
            "Interface sketch"
        } else {
            "Meeting notes"
        };
        let mut controller = controller.borrow_mut();
        let preview = demo_preview(title);
        let overview = raster_image(preview.width, preview.height, &preview.rgba);
        controller.viewer.set_preview(preview, overview);
        // Extended canvases open fit-to-page so their whole shape is visible.
        if demo_long || demo_landscape {
            controller.viewer.set_fit(Fit::Page);
        }
        controller.viewer_screen = true;
    } else if let Some(id) = open_document {
        let mut controller = controller.borrow_mut();
        controller.state.paired = Some(true);
        controller.viewer_screen = true;
        controller.opening_id = Some(id.clone());
        controller.send(Command::Open { id });
    } else if !demo {
        // Pairing is shown until the keyring check reports a saved token.
        let mut controller = controller.borrow_mut();
        controller.pairing_screen = true;
        controller.send(Command::Connect);
    }
    controller.borrow_mut().project(&window);
    window.invoke_focus_library();
    if demo_selected && !demo_pairing {
        // Pick the first visible document exactly as a click would.
        let id = {
            let c = controller.borrow();
            c.state
                .visible()
                .into_iter()
                .map(|index| &c.state.entries[index])
                .find(|entry| entry.kind == "document")
                .map(|entry| entry.id.clone())
        };
        if let Some(id) = id {
            window.invoke_select_entry(id.into());
        }
        window.invoke_demo_context_menu();
    }
    if demo_new_folder && !demo_pairing {
        // `--demo-folder-name <text>` types another name, e.g. an existing
        // folder's, to show a validation message.
        let name = args
            .windows(2)
            .find(|p| p[0] == "--demo-folder-name")
            .map_or_else(|| "Project notes".to_owned(), |p| p[1].clone());
        let mut c = controller.borrow_mut();
        if c.state.open_folder_dialog(String::new()) {
            c.state.edit_folder_name(name);
        }
        c.project(&window);
    }
    if demo_drag && !demo_pairing {
        let drag = window.global::<DragState>();
        drag.on_accepts(|_, _| true);
        drag.invoke_begin("demo".into(), "Meeting notes".into(), false);
        drag.invoke_track(560.0, 250.0);
        drag.invoke_offer("demo".into(), "Projects".into());
    }
    if demo_settings && !demo_pairing {
        let mut c = controller.borrow_mut();
        c.settings_visible = true;
        c.project(&window);
    }
    if let Some(path) = screenshot {
        schedule_screenshot(&window, PathBuf::from(path));
    }
    window.run().context("Slint event loop failed")
}

fn install_callbacks(window: &AppWindow, controller: &Rc<RefCell<Controller>>) {
    macro_rules! callback { ($name:ident, $($arg:ident:$ty:ty),* => $body:expr) => {{ let c=controller.clone(); let weak=window.as_weak(); window.$name(move |$($arg:$ty),*| { if let Some(w)=weak.upgrade(){ let mut c=c.borrow_mut(); $body(&mut c,&w); c.project(&w); } }); }} }
    callback!(on_search_changed, text:SharedString => |c:&mut Controller,_:&AppWindow| c.state.search=text.into());
    callback!(on_filter_changed, value:SharedString => |c:&mut Controller,_:&AppWindow| {
        let scope = Scope::from_str(value.as_str());
        if scope != c.state.scope {
            c.state.scope = scope;
            c.state.selected = None;
            // A flat filter leaves the folder hierarchy; returning to "My
            // files" starts again at the root rather than a stale folder.
            c.state.folder.clear();
        }
    });
    callback!(on_sort_changed, value:SharedString => |c:&mut Controller,_:&AppWindow| c.set_sort(Sort::from_str(value.as_str())));
    callback!(on_view_changed, covers:bool => |c:&mut Controller,_:&AppWindow| c.state.view=if covers{View::Covers}else{View::List});
    callback!(on_navigate, id:SharedString => |c:&mut Controller,_:&AppWindow| c.state.navigate(id.into()));
    callback!(on_go_back, => |c:&mut Controller,_:&AppWindow| c.state.go_back());
    {
        let c = controller.clone();
        let weak = window.as_weak();
        window.on_select_entry(move |id| {
            let Some(window) = weak.upgrade() else { return };
            select(&mut c.borrow_mut(), &window, Some(id.to_string()));
        });
    }
    // "Cancel" in the toolbar and Escape both clear the selection without
    // rebuilding the models: only the two affected rows change.
    {
        let c = controller.clone();
        let weak = window.as_weak();
        window.on_clear_selection(move || {
            let Some(window) = weak.upgrade() else { return };
            select(&mut c.borrow_mut(), &window, None);
        });
    }
    // A right-click selects the entry first, so the menu's enabled flags are
    // the same capability properties the toolbar reads.
    {
        let c = controller.clone();
        let weak = window.as_weak();
        window.on_context_menu_requested(move |id| {
            let Some(window) = weak.upgrade() else { return };
            select(&mut c.borrow_mut(), &window, Some(id.to_string()));
        });
    }
    callback!(on_activate_entry, id:SharedString => |c:&mut Controller,w:&AppWindow| activate(c,w,id.into()));
    callback!(on_open_selected, => |c:&mut Controller,w:&AppWindow| {if let Some(id)=c.state.selected.clone(){activate(c,w,id)}});
    callback!(on_refresh, => |c:&mut Controller,_:&AppWindow| c.send(Command::Refresh));
    callback!(on_import_requested, => |c:&mut Controller,_:&AppWindow| c.send(Command::Import{parent:c.state.folder.clone()}));
    callback!(on_move_requested, => |c:&mut Controller,_:&AppWindow| {c.state.move_source=c.state.selected.clone();c.state.move_visible=c.state.move_source.is_some();});
    callback!(on_move_to, parent:SharedString => |c:&mut Controller,_:&AppWindow| {if let Some(id)=c.state.move_source.take(){c.state.move_visible=false;c.send(Command::Move{id,parent:parent.into()});}});
    callback!(on_move_cancelled, => |c:&mut Controller,_:&AppWindow| {c.state.move_visible=false;c.state.move_source=None;});
    // Drag and drop: Slint asks on every pointer move over a folder, so the
    // check only reads the state.
    {
        let drag = window.global::<DragState>();
        drag.set_token(slint::DataTransfer::from(SharedString::from(
            "turborm-entry",
        )));
        let c = controller.clone();
        drag.on_accepts(move |id, folder| {
            c.try_borrow()
                .is_ok_and(|c| c.state.drop_target(&id, &folder).is_some())
        });
        let c = controller.clone();
        let weak = window.as_weak();
        drag.on_move_into(move |id, folder| {
            let Some(window) = weak.upgrade() else { return };
            let mut c = c.borrow_mut();
            if let Some(parent) = c.state.drop_target(&id, &folder) {
                c.send(Command::Move {
                    id: id.into(),
                    parent,
                });
                c.project(&window);
            }
        });
    }
    callback!(on_new_folder_requested, => |c:&mut Controller,_:&AppWindow| {
        // The toolbar only offers this while browsing "My files"; the check is
        // repeated here so a stale click cannot open it over a flat filter.
        if c.state.scope == Scope::Folder {
            let parent = c.state.folder.clone();
            c.state.open_folder_dialog(parent);
        }
    });
    callback!(on_new_folder_inside_requested, => |c:&mut Controller,_:&AppWindow| {
        if let Some(parent) = c.state.selected.clone() {
            c.state.open_folder_dialog(parent);
        }
    });
    {
        // Typing only updates the name and its message: no model rebuild, so
        // the text field keeps its caret and selection.
        let c = controller.clone();
        let weak = window.as_weak();
        window.on_folder_name_edited(move |text| {
            let Some(window) = weak.upgrade() else { return };
            let mut c = c.borrow_mut();
            c.state.edit_folder_name(text.into());
            if let Some(dialog) = &c.state.folder_dialog {
                window.set_folder_dialog_error(dialog.error.clone().into());
            }
        });
    }
    callback!(on_folder_create_confirmed, => |c:&mut Controller,_:&AppWindow| create_folder(c));
    callback!(on_folder_dialog_cancelled, => |c:&mut Controller,_:&AppWindow| c.state.folder_dialog = None);
    callback!(on_export_original, => |c:&mut Controller,_:&AppWindow| {if let Some(id)=c.state.selected.clone(){c.send(Command::ExportOriginal{id})}});
    callback!(on_export_native, => |c:&mut Controller,_:&AppWindow| {if let Some(id)=c.state.selected.clone(){c.send(Command::ExportNative{id})}});
    callback!(on_export_pdf, => |c:&mut Controller,_:&AppWindow| export_document(c, ExportFormat::PreviewPdf));
    callback!(on_export_svg, => |c:&mut Controller,_:&AppWindow| export_document(c, ExportFormat::Svg));
    callback!(on_verify_import, => |c:&mut Controller,_:&AppWindow| {if c.state.demo {c.state.status="Import verification is unavailable in offline demo mode".into()}else{c.send(Command::VerifyImport)}});
    // "Connect" opens the pairing screen when unpaired, and refreshes otherwise.
    callback!(on_reconnect_requested, => |c:&mut Controller,_:&AppWindow| {
        if c.state.demo {
            c.state.status = "Connection is unavailable in offline demo mode".into();
        } else if c.state.paired == Some(true) {
            c.send(Command::Refresh);
        } else if !c.state.busy {
            c.pairing_screen = true;
            c.send(Command::Connect);
        }
    });
    callback!(on_retry_connect, => |c:&mut Controller,_:&AppWindow| {
        if !c.state.demo && !c.state.busy {
            c.send(Command::Connect);
        }
    });
    {
        let c = controller.clone();
        let weak = window.as_weak();
        window.on_pair(move |code| {
            let Some(window) = weak.upgrade() else { return };
            let mut c = c.borrow_mut();
            if c.state.demo || c.state.busy {
                return;
            }
            // The code is moved into the service command and cleared from the
            // field immediately; it is never stored in Rust state.
            let code = Zeroizing::new(code.to_string());
            if code.len() == 8 {
                c.send(Command::Pair(code));
                window.invoke_clear_pair_code();
            } else {
                c.state.error = "Enter the 8-character code from the connection page.".into();
            }
            c.project(&window);
        });
    }
    window.on_open_connect_page(open_connect_page);
    {
        let c = controller.clone();
        let weak = window.as_weak();
        window.on_viewport_changed(move |width| {
            if !width.is_finite() {
                return;
            }
            let Some(window) = weak.upgrade() else { return };
            let mut c = c.borrow_mut();
            let width = width.max(320.0);
            let (old_row, new_row) = (folder_row_width(c.viewport_width), folder_row_width(width));
            let old_folder = pack_rows(&c.folder_widths, old_row);
            let new_folder = pack_rows(&c.folder_widths, new_row);
            // A tile wider than the row is cut to fit it, so its width moves.
            let clipped = c
                .folder_widths
                .iter()
                .any(|&tile| tile > old_row.min(new_row));
            let old_cover =
                (((c.viewport_width - 14.0).max(COVER_CELL) / COVER_CELL).floor() as usize).max(1);
            let new_cover = (((width - 14.0).max(COVER_CELL) / COVER_CELL).floor() as usize).max(1);
            c.viewport_width = width;
            if old_folder != new_folder || clipped || old_cover != new_cover {
                c.project(&window);
            }
        });
    }
    {
        let c = controller.clone();
        let weak = window.as_weak();
        window.on_cover_visible(move |id| {
            let c = c.clone();
            let weak = weak.clone();
            slint::Timer::single_shot(Duration::ZERO, move || {
                let Some(w) = weak.upgrade() else { return };
                let mut c = c.borrow_mut();
                if let Some((id, epoch)) = c.state.request_cover(id.as_str()) {
                    if c.tx
                        .send(Command::Thumbnail {
                            id: id.clone(),
                            epoch,
                        })
                        .is_err()
                    {
                        c.state.finish_cover(id, epoch, Err(()));
                        c.project(&w);
                    }
                }
            });
        });
    }
    {
        let c = controller.clone();
        let weak = window.as_weak();
        window.on_worker_wakeup(move || {
            if let Some(window) = weak.upgrade() {
                c.borrow_mut().drain(&window);
            }
        });
    }
    // Viewer. The library keeps its selection, folder and scroll position while
    // the viewer is on screen; "Library" only flips the screen back.
    callback!(on_viewer_back, => |c:&mut Controller,_:&AppWindow| {
        c.viewer_screen = false;
        c.viewer.clear();
        c.state.status.clear();
    });
    callback!(on_viewer_go_page, page:i32 => |c:&mut Controller,_:&AppWindow| {
        if let Ok(page) = usize::try_from(page) {
            c.go_page(page);
        }
    });
    callback!(on_viewer_page_field_edited, text:SharedString => |c:&mut Controller,_:&AppWindow| {
        match text.trim().parse::<usize>() {
            Ok(value) if value > 0 => c.go_page(value - 1),
            _ => c.viewer.page_field = (c.viewer.page() + 1).to_string(),
        }
    });
    callback!(on_viewer_set_fit, mode:SharedString => |c:&mut Controller,_:&AppWindow| {
        if let Some(fit) = Fit::from_str(mode.as_str()) {
            c.viewer.set_fit(fit);
        }
    });
    callback!(on_viewer_zoom_by, steps:i32 => |c:&mut Controller,_:&AppWindow| c.viewer.zoom_by(steps));
    callback!(on_viewer_set_zoom, percent:i32 => |c:&mut Controller,_:&AppWindow| c.viewer.set_zoom(percent as f32));
    callback!(on_viewer_export_pdf, => |c:&mut Controller,_:&AppWindow| {
        if c.viewer.preview.is_some() {
            c.send(Command::ExportPdf);
        }
    });
    callback!(on_viewer_export_svg, => |c:&mut Controller,_:&AppWindow| {
        if c.viewer.preview.is_some() {
            let page = c.viewer.page();
            c.send(Command::ExportSvg(page));
        }
    });
    callback!(on_viewer_export_svg_pages, => |c:&mut Controller,_:&AppWindow| {
        if c.viewer.preview.is_some() {
            c.send(Command::ExportSvgPages);
        }
    });
    {
        let c = controller.clone();
        let weak = window.as_weak();
        window.on_viewer_viewport_changed(move |x, y, width, height| {
            let Some(window) = weak.upgrade() else { return };
            if !(x.is_finite() && y.is_finite() && width.is_finite() && height.is_finite()) {
                return;
            }
            let mut c = c.borrow_mut();
            let resized = (c.viewer.viewport_width - width).abs() > 0.5
                || (c.viewer.viewport_height - height).abs() > 0.5;
            c.viewer.scroll_x = x.max(0.0);
            c.viewer.scroll_y = y.max(0.0);
            c.viewer.viewport_width = width.max(1.0);
            c.viewer.viewport_height = height.max(1.0);
            // A resize changes the fit scale, so the page geometry is reprojected.
            if resized {
                c.project(&window);
            } else {
                c.sync_tiles(&window);
            }
        });
    }
    // Slint owns the theme flag; Rust only needs to observe the change.
    window.on_theme_toggled(|| {});
    // Settings. Opening the dialog measures the disk again, so its numbers
    // are fresh each time it is shown.
    callback!(on_settings_requested, => |c:&mut Controller,_:&AppWindow| {
        c.settings_visible = true;
        c.storage_used = None;
        c.send_setting(Command::MeasureStorage);
    });
    callback!(on_settings_closed, => |c:&mut Controller,_:&AppWindow| c.settings_visible = false);
    callback!(on_cache_mode_changed, value:SharedString => |c:&mut Controller,_:&AppWindow| {
        if let Some(mode) = CacheMode::from_name(value.as_str()) {
            c.set_cache_mode(mode);
        }
    });
    callback!(on_clear_cache_requested, => |c:&mut Controller,_:&AppWindow| {
        if c.settings.cache != CacheMode::Full {
            c.storage_used = None;
            c.send_setting(Command::ClearCache);
        }
    });
}

/// Opens the device connection page in the desktop browser. No shell is used,
/// and the URL is a fixed constant rather than anything user-supplied.
fn open_connect_page() {
    let _ = Process::new("xdg-open")
        .arg(CONNECT_URL)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

fn schedule_screenshot(window: &AppWindow, path: PathBuf) {
    let weak = window.as_weak();
    slint::Timer::single_shot(Duration::from_millis(150), move || {
        let Some(window) = weak.upgrade() else { return };
        let snapshot = window.window().take_snapshot();
        let weak = window.as_weak();
        std::thread::spawn(move || {
            if let Ok(pixels) = snapshot {
                let _ = image::save_buffer(
                    path,
                    pixels.as_bytes(),
                    pixels.width(),
                    pixels.height(),
                    image::ColorType::Rgba8,
                );
            }
            let _ = weak.upgrade_in_event_loop(|_| {
                let _ = slint::quit_event_loop();
            });
        });
    });
}

/// Sets or clears the selection and refreshes only the affected rows and the
/// capability properties the toolbar and context menu read. Rust stays the
/// single owner of the selection.
fn select(c: &mut Controller, window: &AppWindow, id: Option<String>) {
    let old = std::mem::replace(&mut c.state.selected, id.clone());
    if let Some(old) = old {
        c.update_item(&old);
    }
    if let Some(id) = &id {
        c.update_item(id);
    }
    let selected = id.as_deref().and_then(|id| c.state.entry(id));
    let document = selected.is_some_and(|entry| entry.kind == "document");
    window.set_selection_title(
        selected
            .map_or_else(String::new, |entry| entry.title.clone())
            .into(),
    );
    window.set_can_open(document);
    window.set_can_move(selected.is_some() && !c.state.demo);
    window.set_can_export_original(document && !c.state.demo);
    window.set_can_export_native(document && !c.state.demo);
    window.set_can_export_pages(document && !c.state.demo);
    window.set_can_create_inside(c.state.can_create_inside());
}

/// "Export PDF" or "Export SVG" on the selected document, which stays closed.
fn export_document(c: &mut Controller, format: ExportFormat) {
    if c.state.demo {
        c.state.status = "Export is unavailable in offline demo mode".into();
        return;
    }
    let Some(id) = c.state.selected.clone() else {
        return;
    };
    if c.state
        .entry(&id)
        .is_some_and(|entry| entry.kind == "document")
    {
        c.send(Command::ExportDocument { id, format });
    }
}

/// "Create" in the New folder dialog: validate in Rust, then create the folder
/// in the cloud, or only in memory in the offline demo.
fn create_folder(c: &mut Controller) {
    if c.state.busy {
        return;
    }
    let Some((parent, name)) = c.state.confirm_folder_dialog() else {
        return;
    };
    if c.state.demo {
        c.state.create_demo_folder(parent, name);
        c.state.error.clear();
        c.state.status = "Folder created (offline demo; nothing was sent)".into();
        return;
    }
    c.send(Command::CreateFolder { parent, name });
    if c.state.busy {
        c.state.status = "Creating folder…".into();
    }
}

/// Opens a folder in the library, or a document in the viewer screen. The
/// library keeps its state; only the visible screen changes.
fn activate(c: &mut Controller, window: &AppWindow, id: String) {
    let Some(entry) = c.state.entry(&id) else {
        return;
    };
    if entry.kind == "folder" {
        c.state.navigate(id);
    } else if entry.kind == "document" && !c.state.busy {
        let title = entry.title.clone();
        c.viewer.clear();
        c.viewer_screen = true;
        if c.state.demo {
            let preview = demo_preview(&title);
            let overview = raster_image(preview.width, preview.height, &preview.rgba);
            c.viewer.set_preview(preview, overview);
        } else {
            c.opening_id = Some(id.clone());
            c.send(Command::Open { id });
        }
        window.invoke_focus_viewer();
    }
}

/// A synthetic ruled page for the offline demo modes. No cloud data is used.
fn demo_preview(title: &str) -> Preview {
    let landscape = title.contains("Interface") || title.contains("landscape");
    let long = title.contains("Research") || title.contains("long");
    let (canvas_width, canvas_height): (f32, f32) = if landscape {
        (1872.0, 1404.0)
    } else if long {
        (1404.0, 7200.0)
    } else {
        (1404.0, 1872.0)
    };
    let raster_scale = 1800.0 / canvas_height.max(canvas_width);
    let width = (canvas_width * raster_scale).round().max(1.0) as usize;
    let height = (canvas_height * raster_scale).round().max(1.0) as usize;
    let mut rgba = vec![255u8; width * height * 4];
    let margin_x = (100.0 * raster_scale).round().max(1.0) as usize;
    let margin_y = (100.0 * raster_scale).round().max(1.0) as usize;
    let spacing = (100.0 * raster_scale).round().max(1.0) as usize;
    if width > margin_x * 2 && height > margin_y * 2 {
        for y in (margin_y..height - margin_y).step_by(spacing) {
            for x in margin_x..width - margin_x {
                let pixel = (y * width + x) * 4;
                rgba[pixel..pixel + 3].copy_from_slice(&[190, 195, 205]);
            }
        }
    }
    Preview {
        rgba,
        width,
        height,
        page: 0,
        total: 3,
        title: title.to_string(),
        // One of each severity, so the demo shows both the counted warning and
        // the muted Notes section.
        warnings: vec![
            "Demo preview · no cloud data was accessed.".into(),
            format!(
                "{}Pen brush texture and blending are approximated with variable-width round strokes",
                turborm_core::notebook::NOTE_PREFIX
            ),
        ],
        document_key: format!("demo-{title}"),
        canvas_width,
        canvas_height,
    }
}

/// The card/row subtitle: the page count when the `.content` descriptor gave
/// one, otherwise the modified date as before.
fn subtitle(entry: &Entry) -> String {
    if entry.kind == "folder" {
        return "Folder".into();
    }
    if let Some(pages) = entry.page_count {
        return format!("{pages} page{}", if pages == 1 { "" } else { "s" });
    }
    entry.modified.as_deref().map_or_else(
        || "Document".into(),
        |m| format!("Modified {}", format_modified(m)),
    )
}

/// The "Type" column value. An unread `.content` descriptor reads "Document"
/// rather than guessing a type.
fn type_label(entry: &Entry) -> &'static str {
    if entry.kind == "folder" {
        return "Folder";
    }
    match entry.file_type.as_deref() {
        Some("notebook") => "Notebook",
        Some("pdf") => "PDF",
        Some("epub") => "EPUB",
        _ => "Document",
    }
}

/// Render the cloud's `lastModified` value (epoch milliseconds as a string)
/// as a UTC calendar date and time. Non-numeric values are shown unchanged so
/// demo and unexpected metadata stay readable.
fn format_modified(raw: &str) -> String {
    let Ok(millis) = raw.trim().parse::<u64>() else {
        return raw.to_owned();
    };
    let secs = millis / 1000;
    let (days, rem) = (secs / 86_400, secs % 86_400);
    let (hour, minute) = (rem / 3600, (rem % 3600) / 60);
    // Civil-from-days (Howard Hinnant), valid for the proleptic Gregorian calendar.
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}")
}

#[cfg(test)]
pub(super) fn test_controller_contract(window: &AppWindow) {
    let entry = |id: &str, kind: &str| Entry {
        id: id.into(),
        title: id.into(),
        kind: kind.into(),
        parent: String::new(),
        deleted: false,
        pinned: false,
        modified: None,
        files: 0,
        size: 0,
        path: String::new(),
        hierarchy_warning: None,
        file_type: None,
        page_count: None,
    };
    let (command_tx, command_rx) = std::sync::mpsc::channel();
    let (event_tx, event_rx) = std::sync::mpsc::channel();
    let mut state = LibraryState::new(false);
    state.paired = Some(true);
    state.replace_entries(vec![
        entry("folder", "folder"),
        entry("document", "document"),
    ]);
    let controller = Rc::new(RefCell::new(Controller::new(state, command_tx, event_rx)));
    window.set_entries(ModelRc::from(controller.borrow().entries.clone()));
    window.set_folder_rows(ModelRc::from(controller.borrow().folder_rows.clone()));
    window.set_cover_rows(ModelRc::from(controller.borrow().cover_rows.clone()));
    window.set_breadcrumbs(ModelRc::from(controller.borrow().breadcrumbs.clone()));
    window.set_move_targets(ModelRc::from(controller.borrow().move_targets.clone()));
    install_callbacks(window, &controller);
    controller.borrow_mut().project(window);
    let row = controller.borrow().cover_row_models[0].clone();
    let (_, epoch) = controller
        .borrow_mut()
        .state
        .request_cover("document")
        .unwrap();
    event_tx
        .send(Event::Thumbnail {
            id: "document".into(),
            epoch,
            preview: Ok(Preview {
                rgba: vec![255; 4],
                width: 1,
                height: 1,
                page: 0,
                total: 1,
                title: "document".into(),
                warnings: Vec::new(),
                document_key: "test".into(),
                canvas_width: 1.0,
                canvas_height: 1.0,
            }),
        })
        .unwrap();
    window.invoke_worker_wakeup();
    assert!(Rc::ptr_eq(&row, &controller.borrow().cover_row_models[0]));
    assert!(row.row_data(0).unwrap().cover_ready);
    window.invoke_select_entry("folder".into());
    assert!(
        controller.borrow().folder_row_models[0]
            .row_data(0)
            .unwrap()
            .selected
    );

    controller.borrow_mut().state.demo = true;
    window.invoke_refresh();
    window.invoke_import_requested();
    window.invoke_verify_import();
    window.invoke_reconnect_requested();
    window.invoke_export_original();
    window.invoke_export_native();
    window.invoke_move_requested();
    window.invoke_move_to("".into());
    assert!(
        command_rx.try_recv().is_err(),
        "demo actions must not send service commands"
    );
}

/// Checks the new library structure: the sidebar filter scopes, the sort
/// control, "Cancel"/Escape clearing the selection, the context-menu callback
/// selecting before the menu opens, and Backspace navigating up one folder.
#[cfg(test)]
pub(super) fn test_folder_layout_contract(window: &AppWindow) {
    let folder = |id: &str, title: &str| Entry {
        id: id.into(),
        title: title.into(),
        kind: "folder".into(),
        parent: String::new(),
        deleted: false,
        pinned: false,
        modified: Some("1000".into()),
        files: 0,
        size: 0,
        path: String::new(),
        hierarchy_warning: None,
        file_type: None,
        page_count: None,
    };
    let (command_tx, _command_rx) = std::sync::mpsc::channel();
    let (_event_tx, event_rx) = std::sync::mpsc::channel();
    let mut state = LibraryState::new(false);
    state.paired = Some(true);
    state.replace_entries(vec![
        folder("a", "Tax"),
        folder("b", "Mechanical engineering lecture notes"),
        folder("c", "日本語の勉強ノート"),
        folder("d", "Comics"),
    ]);
    let controller = Rc::new(RefCell::new(Controller::new(state, command_tx, event_rx)));
    window.set_folder_rows(ModelRc::from(controller.borrow().folder_rows.clone()));
    install_callbacks(window, &controller);
    let row_lengths = || {
        let rows = window.get_folder_rows();
        (0..rows.row_count())
            .map(|row| rows.row_data(row).unwrap().items.row_count())
            .collect::<Vec<_>>()
    };

    // Wide enough for every tile on one row, each as wide as its name.
    window.invoke_viewport_changed(1200.0);
    assert_eq!(row_lengths(), vec![4]);
    let widths = {
        let row = window.get_folder_rows().row_data(0).unwrap().items;
        (0..row.row_count())
            .map(|i| row.row_data(i).unwrap())
            .map(|item| (item.entry_id.to_string(), item.tile_width))
            .collect::<HashMap<_, _>>()
    };
    assert_eq!(widths["a"], FOLDER_MIN);
    assert!(
        widths["b"] > 250.0,
        "the long name is shown whole: {}",
        widths["b"]
    );
    assert!(
        widths["c"] > FOLDER_MIN,
        "characters Inter lacks still get room: {}",
        widths["c"]
    );

    // Narrowing the window re-wraps the rows instead of cutting tiles off.
    window.invoke_viewport_changed(420.0);
    let narrow = row_lengths();
    assert!(narrow.len() > 1, "{narrow:?}");
    assert_eq!(narrow.iter().sum::<usize>(), 4);
    window.invoke_viewport_changed(1200.0);
    assert_eq!(row_lengths(), vec![4]);
}

#[cfg(test)]
pub(super) fn test_structure_contract(window: &AppWindow) {
    let entry =
        |id: &str, kind: &str, parent: &str, file_type: Option<&str>, modified: &str| Entry {
            id: id.into(),
            title: id.into(),
            kind: kind.into(),
            parent: parent.into(),
            deleted: false,
            pinned: id == "notes",
            modified: Some(modified.into()),
            files: 0,
            size: 0,
            path: String::new(),
            hierarchy_warning: None,
            file_type: file_type.map(str::to_owned),
            page_count: file_type.map(|_| 7),
        };
    let (command_tx, command_rx) = std::sync::mpsc::channel();
    let (_event_tx, event_rx) = std::sync::mpsc::channel();
    let mut state = LibraryState::new(false);
    state.paired = Some(true);
    state.replace_entries(vec![
        entry("folder", "folder", "", None, "5000"),
        entry("notes", "document", "", Some("notebook"), "4000"),
        entry("paper", "document", "", Some("pdf"), "3000"),
        entry("book", "document", "", Some("epub"), "2000"),
        entry("inside", "document", "folder", Some("pdf"), "1000"),
    ]);
    let controller = Rc::new(RefCell::new(Controller::new(state, command_tx, event_rx)));
    window.set_entries(ModelRc::from(controller.borrow().entries.clone()));
    window.set_folder_rows(ModelRc::from(controller.borrow().folder_rows.clone()));
    window.set_cover_rows(ModelRc::from(controller.borrow().cover_rows.clone()));
    window.set_breadcrumbs(ModelRc::from(controller.borrow().breadcrumbs.clone()));
    window.set_move_targets(ModelRc::from(controller.borrow().move_targets.clone()));
    install_callbacks(window, &controller);
    controller.borrow_mut().project(window);

    // The root view browses the hierarchy: one folder tile and three covers.
    assert_eq!(window.get_filter(), "");
    assert_eq!(window.get_heading_text(), "My files");
    assert_eq!(window.get_folder_rows().row_count(), 1);
    let visible_ids = |controller: &Rc<RefCell<Controller>>| {
        let c = controller.borrow();
        c.state
            .visible()
            .into_iter()
            .map(|index| c.state.entries[index].id.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        visible_ids(&controller),
        vec!["folder", "notes", "paper", "book"]
    );

    // Each sidebar filter is a flat scope over the whole library, including
    // documents inside folders.
    for (value, expected) in [
        ("notebook", vec!["notes"]),
        ("pdf", vec!["paper", "inside"]),
        ("epub", vec!["book"]),
        ("pinned", vec!["notes"]),
    ] {
        window.invoke_filter_changed(value.into());
        assert_eq!(window.get_filter(), value, "filter property for {value}");
        assert_eq!(visible_ids(&controller), expected, "scope {value}");
        // A flat scope has no folder tiles and a single breadcrumb segment.
        assert_eq!(window.get_folder_rows().row_count(), 0, "scope {value}");
        assert_eq!(window.get_breadcrumbs().row_count(), 1, "scope {value}");
    }
    window.invoke_filter_changed("".into());
    assert_eq!(window.get_heading_text(), "My files");

    // Sort: Rust owns it and the projection reorders accordingly.
    assert_eq!(window.get_sort(), "modified");
    window.invoke_sort_changed("name".into());
    assert_eq!(window.get_sort(), "name");
    // The chosen order is what the settings file keeps for the next launch.
    assert_eq!(controller.borrow().settings.sort, SortOrder::Name);
    assert_eq!(
        visible_ids(&controller),
        vec!["folder", "book", "notes", "paper"]
    );
    window.invoke_sort_changed("modified".into());
    assert_eq!(
        visible_ids(&controller),
        vec!["folder", "notes", "paper", "book"]
    );

    // The Type column reads the `.content` file type when it is known.
    window.invoke_view_changed(false);
    let types = (0..window.get_entries().row_count())
        .filter_map(|row| window.get_entries().row_data(row))
        .map(|item| item.type_label.to_string())
        .collect::<Vec<_>>();
    assert_eq!(types, vec!["Folder", "Notebook", "PDF", "EPUB"]);
    // The subtitle prefers the page count over the modified date.
    let notes_row = window.get_entries().row_data(1).expect("the notebook row");
    assert_eq!(notes_row.subtitle, "7 pages");
    window.invoke_view_changed(true);

    // A right-click selects the entry before the menu is shown, so the menu's
    // enabled flags are the toolbar's capability properties.
    window.invoke_context_menu_requested("paper".into());
    assert_eq!(window.get_selection_title(), "paper");
    assert!(window.get_can_open());
    assert!(window.get_can_export_original());
    assert!(window.get_can_export_native());

    // "Cancel" in the toolbar clears the selection without losing the folder.
    window.invoke_clear_selection();
    assert_eq!(window.get_selection_title(), "");
    assert!(!window.get_can_open());
    assert!(!window.get_can_move());
    assert!(controller.borrow().state.selected.is_none());

    // Backspace maps onto the same "up one folder" callback the screen binds.
    window.invoke_navigate("folder".into());
    assert_eq!(window.get_heading_text(), "folder");
    assert!(window.get_can_go_back());
    assert_eq!(visible_ids(&controller), vec!["inside"]);
    window.invoke_go_back();
    assert!(!window.get_can_go_back());
    assert_eq!(window.get_heading_text(), "My files");

    // None of this touched the cloud.
    assert!(
        command_rx.try_recv().is_err(),
        "filtering, sorting and selection must not send service commands"
    );
}

/// Checks dragging and the closed-document exports: a drop asks Rust before it
/// is accepted, moves through the same command as the Move dialog, and the
/// library exports name the document without opening it.
#[cfg(test)]
pub(super) fn test_drag_and_export_contract(window: &AppWindow) {
    let entry = |id: &str, kind: &str, parent: &str| Entry {
        id: id.into(),
        title: id.into(),
        kind: kind.into(),
        parent: parent.into(),
        deleted: false,
        pinned: false,
        modified: None,
        files: 0,
        size: 0,
        path: String::new(),
        hierarchy_warning: None,
        file_type: None,
        page_count: None,
    };
    let (command_tx, command_rx) = std::sync::mpsc::channel();
    let (_event_tx, event_rx) = std::sync::mpsc::channel();
    let mut state = LibraryState::new(false);
    state.paired = Some(true);
    state.replace_entries(vec![
        entry("work", "folder", ""),
        entry("notes", "document", ""),
    ]);
    let controller = Rc::new(RefCell::new(Controller::new(state, command_tx, event_rx)));
    window.set_entries(ModelRc::from(controller.borrow().entries.clone()));
    window.set_folder_rows(ModelRc::from(controller.borrow().folder_rows.clone()));
    window.set_cover_rows(ModelRc::from(controller.borrow().cover_rows.clone()));
    window.set_breadcrumbs(ModelRc::from(controller.borrow().breadcrumbs.clone()));
    window.set_move_targets(ModelRc::from(controller.borrow().move_targets.clone()));
    install_callbacks(window, &controller);
    controller.borrow_mut().project(window);

    let drag = window.global::<DragState>();
    assert!(drag.get_enabled());
    assert!(drag.invoke_accepts("notes".into(), "work".into()));
    // Where it already is, and a folder into itself, are refused.
    assert!(!drag.invoke_accepts("notes".into(), ".".into()));
    assert!(!drag.invoke_accepts("work".into(), "work".into()));
    drag.invoke_move_into("notes".into(), "work".into());
    match command_rx.try_recv() {
        Ok(Command::Move { id, parent }) => {
            assert_eq!((id.as_str(), parent.as_str()), ("notes", "work"))
        }
        _ => panic!("a drop must send Command::Move"),
    }
    // Sending marked the controller busy, so nothing more can be dragged.
    assert!(!drag.get_enabled());
    drag.invoke_move_into("notes".into(), "work".into());
    assert!(command_rx.try_recv().is_err());

    controller.borrow_mut().state.busy = false;
    window.invoke_select_entry("notes".into());
    assert!(window.get_can_export_pages());
    for (export, expected) in [
        (
            AppWindow::invoke_export_pdf as fn(&AppWindow),
            ExportFormat::PreviewPdf,
        ),
        (AppWindow::invoke_export_svg, ExportFormat::Svg),
    ] {
        controller.borrow_mut().state.busy = false;
        export(window);
        match command_rx.try_recv() {
            Ok(Command::ExportDocument { id, format }) => {
                assert_eq!(id, "notes");
                assert_eq!(format, expected);
            }
            _ => panic!("library export must send Command::ExportDocument"),
        }
    }
    // A folder has no pages to export.
    controller.borrow_mut().state.busy = false;
    window.invoke_select_entry("work".into());
    assert!(!window.get_can_export_pages());
    window.invoke_export_svg();
    assert!(command_rx.try_recv().is_err());
}

/// Checks the viewer: opening a document switches the screen, page navigation
/// updates the page property, tile rows are updated in place, and going back
/// leaves the library's selection and folder untouched.
#[cfg(test)]
pub(super) fn test_viewer_contract(window: &AppWindow) {
    use turborm::renderer::{TileKey, TILE_SIZE};

    let entry = |id: &str, kind: &str, parent: &str| Entry {
        id: id.into(),
        title: id.into(),
        kind: kind.into(),
        parent: parent.into(),
        deleted: false,
        pinned: false,
        modified: None,
        files: 0,
        size: 0,
        path: String::new(),
        hierarchy_warning: None,
        file_type: None,
        page_count: None,
    };
    let (command_tx, command_rx) = std::sync::mpsc::channel();
    let (event_tx, event_rx) = std::sync::mpsc::channel();
    let mut state = LibraryState::new(false);
    state.paired = Some(true);
    state.replace_entries(vec![
        entry("folder", "folder", ""),
        entry("notes", "document", ""),
        entry("other", "document", ""),
    ]);
    let controller = Rc::new(RefCell::new(Controller::new(state, command_tx, event_rx)));
    window.set_entries(ModelRc::from(controller.borrow().entries.clone()));
    window.set_folder_rows(ModelRc::from(controller.borrow().folder_rows.clone()));
    window.set_cover_rows(ModelRc::from(controller.borrow().cover_rows.clone()));
    window.set_breadcrumbs(ModelRc::from(controller.borrow().breadcrumbs.clone()));
    window.set_move_targets(ModelRc::from(controller.borrow().move_targets.clone()));
    window.set_viewer_tiles(ModelRc::from(controller.borrow().tiles.clone()));
    install_callbacks(window, &controller);
    controller.borrow_mut().project(window);

    // Selecting keeps the library on screen; activating opens the viewer.
    window.invoke_select_entry("notes".into());
    assert_eq!(window.get_screen(), "library");
    window.invoke_activate_entry("notes".into());
    assert_eq!(window.get_screen(), "viewer");
    match command_rx.try_recv() {
        Ok(Command::Open { id }) => assert_eq!(id, "notes"),
        _ => panic!("opening a document must send Command::Open"),
    }

    // The rendered page arrives through the worker channel.
    let preview = || Preview {
        rgba: vec![255; 4 * 200 * 300],
        width: 200,
        height: 300,
        page: 0,
        total: 4,
        title: "notes".into(),
        warnings: vec![
            "demo warning".into(),
            format!("{}demo note", turborm_core::notebook::NOTE_PREFIX),
        ],
        document_key: "notes-key".into(),
        canvas_width: 1404.0,
        canvas_height: 1872.0,
    };
    event_tx.send(Event::Preview(preview())).expect("queue");
    event_tx.send(Event::Finished).expect("queue");
    window.invoke_worker_wakeup();
    // The page sharpens without waiting for a scroll or a resize.
    assert!(
        std::iter::from_fn(|| command_rx.try_recv().ok())
            .any(|command| matches!(command, Command::Tiles(keys) if !keys.is_empty())),
        "a page that has just arrived must request its tiles"
    );
    assert_eq!(window.get_document_title(), "notes");
    assert_eq!(window.get_viewer_total(), 4);
    assert_eq!(window.get_viewer_page_field(), "1");
    assert!(window.get_viewer_has_document());
    assert!(window.get_viewer_page_width() > 0.0);
    // Content problems and informational notes reach separate properties, so
    // the strip can count only the former.
    assert_eq!(window.get_viewer_warnings().row_count(), 1);
    assert_eq!(window.get_viewer_notes().row_count(), 1);

    // Page navigation goes through the service and updates the page property.
    window.invoke_viewer_go_page(2);
    match command_rx.try_recv() {
        Ok(Command::Page(page)) => assert_eq!(page, 2),
        _ => panic!("page navigation must send Command::Page"),
    }
    let mut second = preview();
    second.page = 2;
    event_tx.send(Event::Preview(second)).expect("queue");
    event_tx.send(Event::Finished).expect("queue");
    window.invoke_worker_wakeup();
    assert_eq!(window.get_viewer_page(), 2);
    assert_eq!(window.get_viewer_page_field(), "3");

    // Zoom controls stay in Rust and reach the generated properties.
    window.invoke_viewer_set_zoom(100);
    assert_eq!(window.get_viewer_fit_mode(), "custom");
    assert_eq!(window.get_viewer_zoom_percent(), 100);
    window.invoke_viewer_set_fit("page".into());
    assert_eq!(window.get_viewer_fit_mode(), "page");
    window.invoke_viewer_set_fit("width".into());

    // Tiles: the worker's raster becomes a row, and a second tile for the same
    // position updates that row instead of appending a new one.
    window.invoke_viewer_viewport_changed(0.0, 0.0, 900.0, 600.0);
    // Each zoom change asked again at its own density; the last one counts.
    let requested = std::iter::from_fn(|| command_rx.try_recv().ok())
        .filter_map(|command| match command {
            Command::Tiles(keys) => keys.into_iter().next(),
            _ => None,
        })
        .last()
        .expect("a visible page must request a tile");
    assert_eq!(requested.page, 2);
    let tile = |key: TileKey| Tile {
        key,
        rgba: vec![200; TILE_SIZE * TILE_SIZE * 4],
        width: TILE_SIZE,
        height: TILE_SIZE,
    };
    event_tx
        .send(Event::Tile(Ok(tile(requested.clone()))))
        .expect("queue");
    window.invoke_worker_wakeup();
    assert_eq!(window.get_viewer_tiles().row_count(), 1);
    let first_row = window.get_viewer_tiles().row_data(0).expect("first tile");
    assert_eq!(first_row.x, 0.0);
    let rows_before = window.get_viewer_tiles().row_count();
    // Re-delivering the same tile must not grow the model.
    controller.borrow_mut().sync_tiles(window);
    assert_eq!(window.get_viewer_tiles().row_count(), rows_before);

    // The page box itself must follow the page after the user has typed in
    // it: typing breaks Slint's text binding, which once left a typed "100"
    // on screen for every document opened afterwards.
    let page_box = || {
        i_slint_backend_testing::ElementHandle::find_by_accessible_label(window, "Page number")
            .next()
            .expect("the page number box")
    };
    assert_eq!(page_box().accessible_value().as_deref(), Some("3"));
    // The real event loop runs between every step; Slint evaluates `changed`
    // handlers there, so the test runs one pass after each step as well.
    slint::platform::update_timers_and_animations();
    page_box().set_accessible_value("4");
    slint::platform::update_timers_and_animations();
    assert_eq!(page_box().accessible_value().as_deref(), Some("4"));
    let mut first = preview();
    first.page = 0;
    event_tx.send(Event::Preview(first)).expect("queue");
    event_tx.send(Event::Finished).expect("queue");
    window.invoke_worker_wakeup();
    slint::platform::update_timers_and_animations();
    assert_eq!(window.get_viewer_page_field(), "1");
    assert_eq!(page_box().accessible_value().as_deref(), Some("1"));

    // Going back keeps the library's selection and folder intact.
    window.invoke_viewer_back();
    assert_eq!(window.get_screen(), "library");
    assert_eq!(
        controller.borrow().state.selected.as_deref(),
        Some("notes"),
        "returning to the library must keep the selection"
    );
    assert_eq!(window.get_selection_title(), "notes");
    assert!(!window.get_viewer_has_document());
}

/// Checks the New folder flow: the toolbar capability, opening and cancelling
/// the dialog, Rust-owned validation, the command sent with the current folder
/// as parent, selection of the created folder, and local-only demo creation.
#[cfg(test)]
pub(super) fn test_new_folder_contract(window: &AppWindow) {
    let entry = |id: &str, title: &str, kind: &str, parent: &str| Entry {
        id: id.into(),
        title: title.into(),
        kind: kind.into(),
        parent: parent.into(),
        deleted: false,
        pinned: false,
        modified: Some("1000".into()),
        files: 1,
        size: 0,
        path: String::new(),
        hierarchy_warning: None,
        file_type: (kind == "document").then(|| "pdf".to_owned()),
        page_count: None,
    };
    let (command_tx, command_rx) = std::sync::mpsc::channel();
    let (event_tx, event_rx) = std::sync::mpsc::channel();
    let mut state = LibraryState::new(false);
    state.paired = Some(true);
    state.replace_entries(vec![
        entry("reviews", "Claude Review", "folder", ""),
        entry("paper", "Paper", "document", ""),
        entry("inner", "TurboRM", "folder", "reviews"),
    ]);
    let controller = Rc::new(RefCell::new(Controller::new(state, command_tx, event_rx)));
    window.set_entries(ModelRc::from(controller.borrow().entries.clone()));
    window.set_folder_rows(ModelRc::from(controller.borrow().folder_rows.clone()));
    window.set_cover_rows(ModelRc::from(controller.borrow().cover_rows.clone()));
    window.set_breadcrumbs(ModelRc::from(controller.borrow().breadcrumbs.clone()));
    window.set_move_targets(ModelRc::from(controller.borrow().move_targets.clone()));
    install_callbacks(window, &controller);
    controller.borrow_mut().project(window);

    // "My files" folder browsing offers New folder; every flat filter does not.
    assert!(window.get_can_create_folder());
    for value in ["pinned", "notebook", "pdf", "epub"] {
        window.invoke_filter_changed(value.into());
        assert!(!window.get_can_create_folder(), "filter {value}");
        window.invoke_new_folder_requested();
        assert!(!window.get_folder_dialog_visible(), "filter {value}");
    }
    window.invoke_filter_changed("".into());
    assert!(window.get_can_create_folder());

    // Opening and cancelling sends nothing.
    window.invoke_new_folder_requested();
    assert!(window.get_folder_dialog_visible());
    assert_eq!(window.get_folder_dialog_name(), "");
    assert_eq!(window.get_folder_dialog_error(), "");
    assert_eq!(window.get_folder_dialog_location(), "My files");
    window.invoke_folder_dialog_cancelled();
    assert!(!window.get_folder_dialog_visible());
    assert!(command_rx.try_recv().is_err());

    // Validation lives in Rust and reaches the dialog as a message.
    window.invoke_new_folder_requested();
    window.invoke_folder_create_confirmed();
    assert!(
        window.get_folder_dialog_visible(),
        "an empty name keeps the dialog"
    );
    assert_eq!(window.get_folder_dialog_error(), "Enter a folder name");
    let message = |text: &str| {
        window.invoke_folder_name_edited(text.into());
        window.get_folder_dialog_error().to_string()
    };
    assert!(message("a/b").contains("\"/\""));
    assert!(message(&"x".repeat(256)).contains("255 characters"));
    assert_eq!(
        message(" claude REVIEW "),
        "A folder named \u{201c}Claude Review\u{201d} already exists here"
    );
    assert_eq!(
        message("   "),
        "",
        "an empty field is not reported while typing"
    );
    // A folder deeper down or a document with the name does not clash here.
    assert_eq!(message("TurboRM"), "");
    assert_eq!(message("paper"), "");
    window.invoke_folder_dialog_cancelled();
    assert!(command_rx.try_recv().is_err());
    // Start again through the text field itself, as a user would type.
    window.invoke_new_folder_requested();
    let field =
        i_slint_backend_testing::ElementHandle::find_by_accessible_label(window, "Folder name")
            .next()
            .expect("the folder name field");
    field.set_accessible_value("claude review");
    assert!(window.get_folder_dialog_error().contains("already exists"));
    field.set_accessible_value("  Project notes ");
    assert_eq!(window.get_folder_dialog_error(), "");
    window.invoke_folder_create_confirmed();
    match command_rx.try_recv() {
        Ok(Command::CreateFolder { parent, name }) => {
            assert_eq!(parent, "", "the current folder is the root");
            assert_eq!(name, "Project notes", "the name is trimmed");
        }
        _ => panic!("Create must send Command::CreateFolder"),
    }
    assert!(!window.get_folder_dialog_visible());
    assert_eq!(window.get_status_text(), "Creating folder…");
    assert!(window.get_busy());

    // The service refreshes the listing and the new folder is selected.
    let mut entries = controller.borrow().state.entries.clone();
    entries.push(entry("created", "Project notes", "folder", ""));
    event_tx.send(Event::Listing(entries)).expect("queue");
    event_tx
        .send(Event::Message("Folder created".into()))
        .expect("queue");
    event_tx
        .send(Event::FolderCreated("created".into()))
        .expect("queue");
    event_tx.send(Event::Finished).expect("queue");
    window.invoke_worker_wakeup();
    assert_eq!(window.get_status_text(), "Folder created");
    assert_eq!(window.get_selection_title(), "Project notes");
    assert!(!window.get_busy());

    // Inside a folder, the folder being browsed is the parent.
    window.invoke_navigate("reviews".into());
    window.invoke_new_folder_requested();
    assert_eq!(window.get_folder_dialog_location(), "Claude Review");
    window.invoke_folder_name_edited("turborm".into());
    assert!(window.get_folder_dialog_error().contains("already exists"));
    window.invoke_folder_name_edited("Round 2".into());
    window.invoke_folder_create_confirmed();
    match command_rx.try_recv() {
        Ok(Command::CreateFolder { parent, name }) => {
            assert_eq!((parent.as_str(), name.as_str()), ("reviews", "Round 2"));
        }
        _ => panic!("Create inside a folder must send Command::CreateFolder"),
    }
    event_tx.send(Event::Finished).expect("queue");
    window.invoke_worker_wakeup();

    // "New folder inside…" from a folder's context menu uses that folder.
    window.invoke_context_menu_requested("inner".into());
    assert!(window.get_can_create_inside());
    window.invoke_new_folder_inside_requested();
    assert_eq!(window.get_folder_dialog_location(), "TurboRM");
    window.invoke_folder_dialog_cancelled();
    window.invoke_context_menu_requested("created".into());
    window.invoke_select_entry("paper".into());
    assert!(!window.get_can_create_inside(), "a document has no inside");

    // Offline demo: the action is off in the toolbar, and a dialog opened by
    // the demo flag creates the folder in memory only.
    window.invoke_navigate("".into());
    controller.borrow_mut().state.demo = true;
    controller.borrow_mut().project(window);
    assert!(!window.get_can_create_folder());
    window.invoke_new_folder_requested();
    window.invoke_folder_name_edited("Sketches".into());
    window.invoke_folder_create_confirmed();
    assert!(
        command_rx.try_recv().is_err(),
        "demo creation is local only"
    );
    let c = controller.borrow();
    let created = c
        .state
        .entries
        .iter()
        .find(|e| e.title == "Sketches")
        .expect("the demo folder");
    assert_eq!(
        (created.kind.as_str(), created.parent.as_str()),
        ("folder", "")
    );
    assert_eq!(c.state.selected.as_deref(), Some(created.id.as_str()));
    drop(c);
    assert_eq!(window.get_selection_title(), "Sketches");
    assert!(!window.get_folder_dialog_visible());
}

/// Checks that the Settings dialog is wired to the service, never makes the
/// library busy, and shows the full-library download's progress.
#[cfg(test)]
pub(super) fn test_settings_contract(window: &AppWindow) {
    let (command_tx, command_rx) = std::sync::mpsc::channel();
    let (event_tx, event_rx) = std::sync::mpsc::channel();
    let mut state = LibraryState::new(false);
    state.paired = Some(true);
    let controller = Rc::new(RefCell::new(Controller::new(state, command_tx, event_rx)));
    install_callbacks(window, &controller);
    controller.borrow_mut().project(window);
    assert!(!window.get_settings_visible());
    assert_eq!(window.get_cache_mode(), "standard", "the default mode");

    // Opening measures the disk; the answer arrives as an event.
    window.invoke_settings_requested();
    assert!(window.get_settings_visible());
    assert!(matches!(command_rx.try_recv(), Ok(Command::MeasureStorage)));
    assert_eq!(window.get_storage_text(), "Measuring…");
    assert!(!window.get_can_clear_cache());
    event_tx
        .send(Event::Storage(450 * 1024 * 1024))
        .expect("queue");
    event_tx
        .send(Event::LibrarySize(3 * 1024 * 1024 * 1024))
        .expect("queue");
    window.invoke_worker_wakeup();
    assert_eq!(window.get_storage_text(), "Downloaded documents use 450 MB");
    assert_eq!(window.get_library_size_text(), "about 3.0 GB");
    assert!(window.get_can_clear_cache());
    assert!(!window.get_busy(), "settings never make the library busy");

    // Choosing a mode sends it at once; choosing it again sends nothing.
    window.invoke_cache_mode_changed("full".into());
    assert!(matches!(
        command_rx.try_recv(),
        Ok(Command::SetCacheMode(CacheMode::Full))
    ));
    window.invoke_cache_mode_changed("full".into());
    window.invoke_cache_mode_changed("huge".into());
    assert!(command_rx.try_recv().is_err());
    assert_eq!(window.get_cache_mode(), "full");
    assert!(!window.get_can_clear_cache(), "full mode keeps its files");
    assert_eq!(window.get_offline_summary(), "saving offline");

    // Progress reaches the dialog and the sidebar.
    let mib = 1024 * 1024;
    event_tx
        .send(Event::Offline(Offline::Saving {
            done: 512 * mib,
            total: 2048 * mib,
        }))
        .expect("queue");
    window.invoke_worker_wakeup();
    assert_eq!(
        window.get_offline_text(),
        "Downloading your library: 512 MB of 2.0 GB (25%)"
    );
    assert_eq!(window.get_offline_summary(), "saving offline 25%");
    event_tx
        .send(Event::Offline(Offline::Stopped(
            "Paused to keep 2.0 GB of disk free".into(),
        )))
        .expect("queue");
    window.invoke_worker_wakeup();
    assert!(window.get_offline_warning());
    assert!(
        matches!(command_rx.try_recv(), Ok(Command::MeasureStorage)),
        "a finished pass measures the disk again"
    );

    // Back to a smaller mode: no progress, and the cache can be cleared.
    window.invoke_cache_mode_changed("small".into());
    assert!(matches!(
        command_rx.try_recv(),
        Ok(Command::SetCacheMode(CacheMode::Small))
    ));
    assert_eq!(window.get_offline_text(), "");
    assert_eq!(window.get_offline_summary(), "");
    assert!(!window.get_offline_warning());
    window.invoke_clear_cache_requested();
    assert!(matches!(command_rx.try_recv(), Ok(Command::ClearCache)));
    assert_eq!(window.get_storage_text(), "Measuring…");

    window.invoke_settings_closed();
    assert!(!window.get_settings_visible());
    assert!(command_rx.try_recv().is_err());
}

/// Checks that the pairing screen is wired to the service and that a successful
/// pairing event returns the window to the library.
#[cfg(test)]
pub(super) fn test_pairing_contract(window: &AppWindow) {
    let (command_tx, command_rx) = std::sync::mpsc::channel();
    let (event_tx, event_rx) = std::sync::mpsc::channel();
    let mut state = LibraryState::new(false);
    state.paired = Some(false);
    let controller = Rc::new(RefCell::new(Controller::new(state, command_tx, event_rx)));
    controller.borrow_mut().pairing_screen = true;
    window.set_entries(ModelRc::from(controller.borrow().entries.clone()));
    window.set_folder_rows(ModelRc::from(controller.borrow().folder_rows.clone()));
    window.set_cover_rows(ModelRc::from(controller.borrow().cover_rows.clone()));
    window.set_breadcrumbs(ModelRc::from(controller.borrow().breadcrumbs.clone()));
    window.set_move_targets(ModelRc::from(controller.borrow().move_targets.clone()));
    install_callbacks(window, &controller);
    controller.borrow_mut().project(window);

    assert_eq!(window.get_screen(), "pairing");
    assert!(!window.get_paired());

    // A short code is rejected in Rust and never reaches the service.
    window.invoke_pair("ABC".into());
    assert!(
        command_rx.try_recv().is_err(),
        "a short code must not be sent to the service"
    );
    assert!(!window.get_error_text().is_empty());

    // A full code is handed to the service as a Pair command.
    window.invoke_pair("ABCD1234".into());
    match command_rx.try_recv() {
        Ok(Command::Pair(code)) => assert_eq!(&*code, "ABCD1234"),
        _ => panic!("an 8-character code must send Command::Pair"),
    }
    assert_eq!(
        window.get_code(),
        "",
        "the one-time code must not be retained in the field"
    );

    // A successful pairing event returns the window to the library.
    event_tx.send(Event::Paired(true)).expect("queue the event");
    window.invoke_worker_wakeup();
    assert_eq!(window.get_screen(), "library");
    assert!(window.get_paired());

    // Theme state is reachable from Rust for optional persistence.
    window.set_theme_dark(true);
    assert!(window.get_theme_dark());
    window.set_theme_dark(false);
}

#[cfg(test)]
mod modified_format_tests {
    use super::format_modified;

    #[test]
    fn epoch_millis_become_utc_date_and_time() {
        assert_eq!(format_modified("0"), "1970-01-01 00:00");
        assert_eq!(format_modified("1790066321588"), "2026-09-22 08:38");
        assert_eq!(format_modified("951782400000"), "2000-02-29 00:00");
    }

    #[test]
    fn non_numeric_values_pass_through() {
        assert_eq!(format_modified("offline demo"), "offline demo");
    }
}

#[cfg(test)]
mod folder_tile_tests {
    use super::*;

    #[test]
    fn tiles_fit_their_names_within_bounds() {
        let short = folder_tile_width("Tax", false);
        let long = folder_tile_width("Mechanical engineering lecture notes", false);
        assert_eq!(short, FOLDER_MIN, "short names keep the minimum tile");
        assert!(long > 250.0 && long < FOLDER_MAX, "{long}");
        // The pin star takes room beside the name.
        let name = "Software & hardware";
        assert!(folder_tile_width(name, true) > folder_tile_width(name, false));
        assert_eq!(folder_tile_width(&"x".repeat(200), false), FOLDER_MAX);
    }

    #[test]
    fn rows_take_as_many_tiles_as_fit() {
        // 112 + 10 + 112 + 10 + 112 = 356.
        assert_eq!(pack_rows(&[112.0; 3], 356.0), vec![3]);
        assert_eq!(pack_rows(&[112.0; 3], 355.0), vec![2, 1]);
        assert_eq!(pack_rows(&[300.0, 112.0, 112.0], 356.0), vec![1, 2]);
        // A tile wider than the row has a row of its own.
        assert_eq!(pack_rows(&[320.0, 320.0], 200.0), vec![1, 1]);
        assert!(pack_rows(&[], 500.0).is_empty());
    }
}
