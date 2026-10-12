//! Viewer domain state: which page is shown, how it is scaled, which tiles are
//! visible and which tile rasters are cached. Presentation-free and testable.
use slint::Image;
use std::collections::{HashMap, HashSet, VecDeque};
use turborm::renderer::{Preview, TileKey, TILE_SIZE};
use turborm_core::notebook::{self, Severity};

/// The viewer keeps at most this many rasterized tiles, matching the bound the
/// previous viewer used.
pub const TILE_CACHE_LIMIT: usize = 64;

/// Breathing room around the page sheet, matching `canvas-margin` in Slint.
pub const CANVAS_MARGIN: f32 = 28.0;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Fit {
    Width,
    Page,
    Custom,
}

impl Fit {
    pub fn as_str(self) -> &'static str {
        match self {
            Fit::Width => "width",
            Fit::Page => "page",
            Fit::Custom => "custom",
        }
    }

    pub fn from_str(value: &str) -> Option<Self> {
        match value {
            "width" => Some(Fit::Width),
            "page" => Some(Fit::Page),
            "custom" => Some(Fit::Custom),
            _ => None,
        }
    }
}

/// A tile ready for presentation, in logical pixels relative to the page.
#[derive(Clone, Debug, PartialEq)]
pub struct PlacedTile {
    pub id: String,
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

pub struct ViewerState {
    pub preview: Option<Preview>,
    pub overview: Option<Image>,
    /// The last image that was actually painted, kept across a page change so
    /// the canvas never goes blank while the next page is prepared.
    previous: Option<Image>,
    /// The page being prepared, while the previous page is still on screen.
    pending_page: Option<usize>,
    /// Title and page count from `Event::Opening`, before any raster exists.
    opening: Option<(String, usize)>,
    pub fit: Fit,
    /// Percent zoom used by [`Fit::Custom`], and the readout for the fit modes.
    pub zoom: f32,
    pub page_field: String,
    pub demo: bool,
    /// Viewport in logical pixels: scroll offset and visible size.
    pub scroll_x: f32,
    pub scroll_y: f32,
    pub viewport_width: f32,
    pub viewport_height: f32,
    /// The generation a tile must belong to; bumped on page, document or scale
    /// changes so stale worker results are dropped instead of drawn.
    generation: u64,
    scale_milli: u32,
    tiles: HashMap<TileKey, (Image, usize, usize)>,
    tile_order: VecDeque<TileKey>,
    failed: HashSet<TileKey>,
    /// Tiles handed to the worker pool and not yet returned, with the
    /// generation they were requested in.
    in_flight: HashMap<TileKey, u64>,
    pub tile_error: String,
}

impl ViewerState {
    pub fn new(demo: bool) -> Self {
        Self {
            preview: None,
            overview: None,
            previous: None,
            pending_page: None,
            opening: None,
            fit: Fit::Width,
            zoom: 100.0,
            page_field: "1".into(),
            demo,
            scroll_x: 0.0,
            scroll_y: 0.0,
            viewport_width: 900.0,
            viewport_height: 600.0,
            generation: 0,
            scale_milli: 0,
            tiles: HashMap::new(),
            tile_order: VecDeque::new(),
            failed: HashSet::new(),
            in_flight: HashMap::new(),
            tile_error: String::new(),
        }
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Tiles requested from the worker pool and not yet returned.
    pub fn in_flight_tiles(&self) -> usize {
        self.in_flight.len()
    }

    /// The page the viewer is waiting for, while the previous one is shown.
    pub fn pending_page(&self) -> Option<usize> {
        self.pending_page
    }

    /// The image to paint under the tiles: the current page's overview, or
    /// the previous page's while the next one is still being prepared.
    pub fn visible_overview(&self) -> Option<&Image> {
        self.overview.as_ref().or(self.previous.as_ref())
    }

    /// True while the painted image belongs to a page other than the one the
    /// viewer is heading for. The screen dims and the status bar explains.
    pub fn showing_stale_page(&self) -> bool {
        self.pending_page.is_some() && self.overview.is_none() && self.previous.is_some()
    }

    #[cfg(test)]
    pub fn cached_tiles(&self) -> usize {
        self.tiles.len()
    }

    /// Accepts a freshly rendered page and invalidates every cached tile.
    ///
    /// Stale tiles are dropped only here, when the replacement image is in
    /// hand, so the canvas is never cleared to nothing.
    pub fn set_preview(&mut self, preview: Preview, overview: Option<Image>) {
        self.page_field = (preview.page + 1).to_string();
        self.preview = Some(preview);
        if overview.is_some() {
            self.previous = overview.clone();
        }
        self.overview = overview;
        self.pending_page = None;
        self.opening = None;
        self.clear_tiles();
    }

    /// The viewer is switching to `page`; keep showing what is on screen.
    ///
    /// The current image stays visible and the tiles it came with stay valid
    /// until `set_preview` delivers the replacement.
    pub fn begin_page(&mut self, page: usize) {
        if self.preview.as_ref().is_some_and(|p| p.page == page) {
            return;
        }
        self.pending_page = Some(page);
        self.page_field = (page + 1).to_string();
        // Keep painting the page that is on screen; drop only the claim that
        // its raster is the current page's.
        self.previous = self.overview.take().or(self.previous.take());
    }

    /// A document is opening on `page`: show its title, page count and page
    /// number at once, with `placeholder` (the library cover, scaled up)
    /// standing in for the page.
    pub fn begin_opening(
        &mut self,
        title: String,
        pages: usize,
        page: usize,
        placeholder: Option<Image>,
    ) {
        self.opening = Some((title, pages));
        self.pending_page = Some(page);
        self.page_field = (page + 1).to_string();
        self.preview = None;
        self.overview = None;
        if placeholder.is_some() {
            self.previous = placeholder;
        }
        self.clear_tiles();
    }

    pub fn clear(&mut self) {
        self.preview = None;
        self.overview = None;
        self.previous = None;
        self.pending_page = None;
        self.opening = None;
        self.page_field = "1".into();
        self.scroll_x = 0.0;
        self.scroll_y = 0.0;
        self.clear_tiles();
    }

    fn clear_tiles(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.tiles.clear();
        self.tile_order.clear();
        self.failed.clear();
        self.in_flight.clear();
        self.tile_error.clear();
        self.scale_milli = 0;
    }

    /// The page the viewer is on, which is the page being prepared as soon as
    /// navigation starts rather than when its raster arrives.
    pub fn page(&self) -> usize {
        self.pending_page
            .or_else(|| self.preview.as_ref().map(|preview| preview.page))
            .unwrap_or(0)
    }

    pub fn total(&self) -> usize {
        self.preview
            .as_ref()
            .map(|preview| preview.total)
            .or_else(|| self.opening.as_ref().map(|(_, pages)| *pages))
            .unwrap_or(0)
    }

    pub fn title(&self) -> String {
        self.preview
            .as_ref()
            .map(|preview| preview.title.clone())
            .or_else(|| self.opening.as_ref().map(|(title, _)| title.clone()))
            .unwrap_or_else(|| "Opening document…".to_string())
    }

    /// True as soon as there is something to show: a rendered page, or an
    /// opening document whose title and page count are already known.
    pub fn has_document(&self) -> bool {
        self.preview.is_some() || self.opening.is_some()
    }

    /// Content problems on the current page: things that could not be
    /// rendered faithfully. These are what the strip counts.
    pub fn warnings(&self) -> Vec<String> {
        self.split_notes(Severity::Warning)
    }

    /// Informational notes on the current page: approximations and standing
    /// facts. Listed under their own heading and not counted in the badge.
    pub fn notes(&self) -> Vec<String> {
        self.split_notes(Severity::Note)
    }

    fn split_notes(&self, wanted: Severity) -> Vec<String> {
        let Some(preview) = self.preview.as_ref() else {
            return Vec::new();
        };
        preview
            .warnings
            .iter()
            .map(|warning| notebook::severity(warning))
            .filter(|note| note.severity == wanted)
            .map(|note| note.text)
            .collect()
    }

    /// Logical pixels per canvas unit for the current fit mode and viewport.
    pub fn scale(&self) -> f32 {
        let Some(preview) = self.preview.as_ref() else {
            return 1.0;
        };
        let canvas_width = preview.canvas_width.max(1.0);
        let canvas_height = preview.canvas_height.max(1.0);
        // The sheet keeps CANVAS_MARGIN of breathing room on every side.
        let width_scale = ((self.viewport_width - CANVAS_MARGIN * 2.0).max(1.0)) / canvas_width;
        let height_scale = ((self.viewport_height - CANVAS_MARGIN * 2.0).max(1.0)) / canvas_height;
        let scale = match self.fit {
            Fit::Width => width_scale,
            Fit::Page => width_scale.min(height_scale),
            Fit::Custom => self.zoom / 100.0,
        };
        scale.clamp(0.01, 8.0)
    }

    /// The zoom readout: the true scale for a fit mode, the setting otherwise.
    pub fn zoom_percent(&self) -> i32 {
        match self.fit {
            Fit::Custom => self.zoom.round() as i32,
            _ if self.preview.is_some() => (self.scale() * 100.0).round() as i32,
            _ => self.zoom.round() as i32,
        }
    }

    pub fn page_size(&self) -> (f32, f32) {
        let Some(preview) = self.preview.as_ref() else {
            return (0.0, 0.0);
        };
        let scale = self.scale();
        (
            (preview.canvas_width.max(1.0) * scale).max(1.0),
            (preview.canvas_height.max(1.0) * scale).max(1.0),
        )
    }

    /// Switches the fit mode, keeping the readout meaningful for custom zoom.
    pub fn set_fit(&mut self, fit: Fit) {
        if fit == Fit::Custom && self.fit != Fit::Custom {
            self.zoom = (self.scale() * 100.0).clamp(25.0, 300.0);
        }
        self.fit = fit;
    }

    pub fn set_zoom(&mut self, percent: f32) {
        self.zoom = percent.clamp(25.0, 300.0);
        self.fit = Fit::Custom;
    }

    /// One zoom step, starting from the current effective scale.
    pub fn zoom_by(&mut self, steps: i32) {
        let current = if self.fit == Fit::Custom {
            self.zoom
        } else {
            (self.scale() * 100.0).clamp(25.0, 300.0)
        };
        let factor = 1.25_f32.powi(steps);
        self.set_zoom((current * factor).round());
    }

    /// The effective raster density, quantized so scrolling does not rerender.
    pub fn raster_scale_milli(&self, dpi: f32) -> u32 {
        (self.scale() * dpi.max(0.1) * 1000.0)
            .round()
            .clamp(10.0, 8000.0) as u32
    }

    /// Applies the current raster density, dropping stale tiles when it moved.
    pub fn sync_scale(&mut self, scale_milli: u32) {
        if self.scale_milli == scale_milli {
            return;
        }
        self.scale_milli = scale_milli;
        self.generation = self.generation.wrapping_add(1);
        self.tiles.clear();
        self.tile_order.clear();
        self.failed.clear();
        self.in_flight.clear();
        self.tile_error.clear();
    }

    /// The tile grid covering the visible region, in page-local coordinates.
    pub fn visible_tile_keys(&self) -> Vec<TileKey> {
        let Some(preview) = self.preview.as_ref() else {
            return Vec::new();
        };
        if self.scale_milli == 0 {
            return Vec::new();
        }
        let scale = self.scale();
        let raster = self.scale_milli as f32 / 1000.0;
        let (page_width, page_height) = self.page_size();
        // Page-local visible rectangle in logical pixels.
        let left = (self.scroll_x - CANVAS_MARGIN).max(0.0).min(page_width);
        let top = (self.scroll_y - CANVAS_MARGIN).max(0.0).min(page_height);
        let right = (left + self.viewport_width).min(page_width);
        let bottom = (top + self.viewport_height).min(page_height);
        if right <= left || bottom <= top {
            return Vec::new();
        }
        let density = raster / scale.max(0.0001);
        let tile = TILE_SIZE as f32;
        let first_column = ((left * density) / tile).floor().max(0.0) as u32;
        let first_row = ((top * density) / tile).floor().max(0.0) as u32;
        let last_column = (((right * density) / tile).ceil() as u32).saturating_sub(1);
        let last_row = (((bottom * density) / tile).ceil() as u32).saturating_sub(1);
        let max_column = ((preview.canvas_width.max(1.0) * raster).ceil() as u32).saturating_sub(1)
            / TILE_SIZE as u32;
        let max_row = ((preview.canvas_height.max(1.0) * raster).ceil() as u32).saturating_sub(1)
            / TILE_SIZE as u32;
        let mut keys = Vec::new();
        for row in first_row..=last_row.min(max_row) {
            for column in first_column..=last_column.min(max_column) {
                keys.push(TileKey {
                    document_key: preview.document_key.clone(),
                    page: preview.page,
                    scale_milli: self.scale_milli,
                    column,
                    row,
                });
                // A viewport never needs more tiles than the cache holds.
                if keys.len() >= TILE_CACHE_LIMIT {
                    return keys;
                }
            }
        }
        keys
    }

    /// Placement of the cached tiles among the visible keys, for the model.
    pub fn placed_tiles(&self) -> Vec<(PlacedTile, Image)> {
        let scale = self.scale();
        let raster = (self.scale_milli as f32 / 1000.0).max(0.0001);
        let points_per_pixel = scale / raster;
        self.visible_tile_keys()
            .into_iter()
            .filter_map(|key| {
                let (image, width, height) = self.tiles.get(&key)?;
                Some((
                    PlacedTile {
                        id: tile_id(&key),
                        x: key.column as f32 * TILE_SIZE as f32 * points_per_pixel,
                        y: key.row as f32 * TILE_SIZE as f32 * points_per_pixel,
                        width: *width as f32 * points_per_pixel,
                        height: *height as f32 * points_per_pixel,
                    },
                    image.clone(),
                ))
            })
            .collect()
    }

    /// Every visible tile still missing, ordered centre-out.
    ///
    /// The whole batch goes to the worker pool in one command; the pool renders
    /// them in this order, so the middle of the viewport sharpens first.
    pub fn next_tile_requests(&mut self) -> Vec<(TileKey, u64)> {
        if self.demo {
            return Vec::new();
        }
        let keys = self.visible_tile_keys();
        if keys.is_empty() {
            return Vec::new();
        }
        // The centre of the visible grid, in tile coordinates.
        let centre_column =
            keys.iter().map(|key| key.column).sum::<u32>() as f32 / keys.len() as f32;
        let centre_row = keys.iter().map(|key| key.row).sum::<u32>() as f32 / keys.len() as f32;
        let mut wanted: Vec<TileKey> = keys
            .into_iter()
            .filter(|key| {
                !self.tiles.contains_key(key)
                    && !self.failed.contains(key)
                    && !self.in_flight.contains_key(key)
            })
            .collect();
        wanted.sort_by(|a, b| {
            let distance = |key: &TileKey| {
                let dx = key.column as f32 - centre_column;
                let dy = key.row as f32 - centre_row;
                dx * dx + dy * dy
            };
            distance(a)
                .total_cmp(&distance(b))
                // A stable order keeps repeated requests identical.
                .then_with(|| (a.row, a.column).cmp(&(b.row, b.column)))
        });
        let generation = self.generation;
        // Never have more outstanding than the cache can hold, so a fast
        // scroll cannot queue unbounded work.
        wanted.truncate(TILE_CACHE_LIMIT.saturating_sub(self.in_flight.len()));
        for key in &wanted {
            self.in_flight.insert(key.clone(), generation);
        }
        wanted.into_iter().map(|key| (key, generation)).collect()
    }

    /// Stores a rendered tile. Returns false for a stale generation.
    pub fn finish_tile(
        &mut self,
        key: TileKey,
        generation: u64,
        result: Result<(Image, usize, usize), String>,
    ) -> bool {
        if self
            .in_flight
            .get(&key)
            .is_some_and(|requested| *requested == generation)
        {
            self.in_flight.remove(&key);
        }
        if generation != self.generation || key.scale_milli != self.scale_milli {
            return false;
        }
        match result {
            Ok((image, width, height)) => {
                if !self.tiles.contains_key(&key) {
                    self.tile_order.push_back(key.clone());
                }
                self.tiles.insert(key, (image, width, height));
                while self.tile_order.len() > TILE_CACHE_LIMIT {
                    if let Some(old) = self.tile_order.pop_front() {
                        self.tiles.remove(&old);
                    }
                }
            }
            Err(error) => {
                self.failed.insert(key);
                self.tile_error = error;
            }
        }
        true
    }
}

/// A stable identity for a tile, used to update model rows in place.
pub fn tile_id(key: &TileKey) -> String {
    format!(
        "{}|{}|{}|{}|{}",
        key.document_key, key.page, key.scale_milli, key.column, key.row
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn preview(width: f32, height: f32) -> Preview {
        Preview {
            rgba: vec![255; 4],
            width: 1,
            height: 1,
            page: 0,
            total: 3,
            title: "Notes".into(),
            warnings: vec!["one".into()],
            document_key: "doc".into(),
            canvas_width: width,
            canvas_height: height,
        }
    }

    fn state(width: f32, height: f32) -> ViewerState {
        let mut state = ViewerState::new(false);
        state.viewport_width = 900.0;
        state.viewport_height = 600.0;
        state.set_preview(preview(width, height), None);
        state
    }

    #[test]
    fn fit_width_uses_the_viewport_width() {
        let state = state(1404.0, 1872.0);
        let (page_width, _) = state.page_size();
        assert!((page_width - 844.0).abs() < 1.0, "page width {page_width}");
    }

    #[test]
    fn fit_page_keeps_the_whole_canvas_inside_the_viewport() {
        let mut state = state(1404.0, 1872.0);
        state.set_fit(Fit::Page);
        let (page_width, page_height) = state.page_size();
        assert!(page_width <= 844.0 + 1.0);
        assert!(page_height <= 544.0 + 1.0);
    }

    #[test]
    fn a_long_canvas_keeps_its_full_height() {
        let state = state(1404.0, 7200.0);
        let (page_width, page_height) = state.page_size();
        assert!((page_height / page_width - 7200.0 / 1404.0).abs() < 0.01);
    }

    #[test]
    fn a_landscape_canvas_keeps_its_aspect_ratio() {
        let state = state(1872.0, 1404.0);
        let (page_width, page_height) = state.page_size();
        assert!((page_width / page_height - 1872.0 / 1404.0).abs() < 0.01);
    }

    #[test]
    fn zoom_steps_stay_inside_the_supported_range() {
        let mut state = state(1404.0, 1872.0);
        for _ in 0..20 {
            state.zoom_by(1);
        }
        assert_eq!(state.zoom_percent(), 300);
        for _ in 0..40 {
            state.zoom_by(-1);
        }
        assert_eq!(state.zoom_percent(), 25);
    }

    #[test]
    fn visible_tiles_follow_the_scroll_offset() {
        let mut state = state(1404.0, 1872.0);
        state.set_fit(Fit::Custom);
        state.set_zoom(100.0);
        state.sync_scale(state.raster_scale_milli(1.0));
        let top = state.visible_tile_keys();
        assert!(top.iter().all(|key| key.row <= 1));
        state.scroll_y = 1200.0;
        let scrolled = state.visible_tile_keys();
        assert!(
            scrolled.iter().all(|key| key.row >= 2),
            "rows {:?}",
            scrolled.iter().map(|k| k.row).collect::<Vec<_>>()
        );
    }

    #[test]
    fn the_tile_cache_is_bounded_at_sixty_four() {
        let mut state = state(1404.0, 7200.0);
        state.sync_scale(1000);
        let image = Image::default();
        for row in 0..80u32 {
            let key = TileKey {
                document_key: "doc".into(),
                page: 0,
                scale_milli: 1000,
                column: 0,
                row,
            };
            assert!(state.finish_tile(key, state.generation(), Ok((image.clone(), 512, 512))));
        }
        assert_eq!(state.cached_tiles(), TILE_CACHE_LIMIT);
    }

    #[test]
    fn a_stale_generation_tile_is_dropped() {
        let mut state = state(1404.0, 1872.0);
        state.sync_scale(1000);
        let stale = state.generation();
        let key = TileKey {
            document_key: "doc".into(),
            page: 0,
            scale_milli: 1000,
            column: 0,
            row: 0,
        };
        state.set_preview(preview(1404.0, 1872.0), None);
        state.sync_scale(1000);
        assert!(!state.finish_tile(key, stale, Ok((Image::default(), 512, 512))));
        assert_eq!(state.cached_tiles(), 0);
    }

    #[test]
    fn every_visible_tile_is_requested_in_one_batch_centre_out() {
        let mut state = state(1404.0, 1872.0);
        state.set_fit(Fit::Custom);
        state.set_zoom(100.0);
        state.sync_scale(state.raster_scale_milli(1.0));
        let visible = state.visible_tile_keys();
        assert!(visible.len() > 1, "the viewport must span several tiles");

        // One batch covers the whole viewport instead of one tile per round.
        let batch = state.next_tile_requests();
        assert_eq!(batch.len(), visible.len());
        assert_eq!(state.in_flight_tiles(), visible.len());
        assert!(batch
            .iter()
            .all(|(_, generation)| *generation == state.generation()));

        // Centre-out: each request is no further from the middle of the grid
        // than the one after it.
        let centre_column =
            visible.iter().map(|key| key.column).sum::<u32>() as f32 / visible.len() as f32;
        let centre_row =
            visible.iter().map(|key| key.row).sum::<u32>() as f32 / visible.len() as f32;
        let distance = |key: &TileKey| {
            let dx = key.column as f32 - centre_column;
            let dy = key.row as f32 - centre_row;
            dx * dx + dy * dy
        };
        for pair in batch.windows(2) {
            assert!(
                distance(&pair[0].0) <= distance(&pair[1].0),
                "tiles must be ordered centre-out"
            );
        }

        // Nothing is requested twice while it is still in flight.
        assert!(state.next_tile_requests().is_empty());
        let (first, generation) = batch[0].clone();
        assert!(state.finish_tile(first.clone(), generation, Ok((Image::default(), 512, 512))));
        assert_eq!(state.in_flight_tiles(), visible.len() - 1);
        // A delivered tile is not asked for again either.
        assert!(state
            .next_tile_requests()
            .iter()
            .all(|(key, _)| *key != first));
    }

    #[test]
    fn a_batch_never_exceeds_the_tile_cache_bound() {
        let mut state = state(1404.0, 200_000.0);
        state.set_fit(Fit::Custom);
        state.set_zoom(300.0);
        state.viewport_width = 4000.0;
        state.viewport_height = 4000.0;
        state.sync_scale(state.raster_scale_milli(1.0));
        let batch = state.next_tile_requests();
        assert!(batch.len() <= TILE_CACHE_LIMIT, "batch was {}", batch.len());
        assert!(state.in_flight_tiles() <= TILE_CACHE_LIMIT);
    }

    #[test]
    fn a_failed_tile_is_not_requested_again() {
        let mut state = state(1404.0, 1872.0);
        state.sync_scale(state.raster_scale_milli(1.0));
        let batch = state.next_tile_requests();
        let (key, generation) = batch[0].clone();
        assert!(state.finish_tile(key.clone(), generation, Err("no renderer".into())));
        assert_eq!(state.tile_error, "no renderer");
        // Every other tile is still wanted; the failed one is not.
        for (other, other_generation) in batch.iter().skip(1) {
            assert!(state.finish_tile(
                other.clone(),
                *other_generation,
                Ok((Image::default(), 512, 512))
            ));
        }
        assert!(state
            .next_tile_requests()
            .iter()
            .all(|(next, _)| *next != key));
    }

    #[test]
    fn the_previous_page_stays_on_screen_until_the_new_preview_lands() {
        let mut state = state(1404.0, 1872.0);
        let first = Image::default();
        state.set_preview(preview(1404.0, 1872.0), Some(first.clone()));
        state.sync_scale(state.raster_scale_milli(1.0));
        let batch = state.next_tile_requests();
        let (key, generation) = batch[0].clone();
        assert!(state.finish_tile(key, generation, Ok((Image::default(), 512, 512))));
        assert_eq!(state.cached_tiles(), 1);

        // Navigating keeps the painted image and its tiles.
        state.begin_page(2);
        assert_eq!(state.pending_page(), Some(2));
        assert_eq!(state.page(), 2, "the viewer is already on the new page");
        assert_eq!(state.page_field, "3");
        assert!(
            state.visible_overview().is_some(),
            "the previous page must stay on screen"
        );
        assert!(state.showing_stale_page(), "and be marked as stale");
        assert_eq!(state.cached_tiles(), 1, "its tiles are not dropped yet");

        // The replacement clears the stale marking and the old tiles together.
        let mut second = preview(1404.0, 1872.0);
        second.page = 2;
        state.set_preview(second, Some(Image::default()));
        assert_eq!(state.pending_page(), None);
        assert!(!state.showing_stale_page());
        assert!(state.visible_overview().is_some());
        assert_eq!(state.cached_tiles(), 0, "stale tiles go with the new page");
    }

    #[test]
    fn opening_shows_the_title_page_count_and_cover_placeholder_at_once() {
        let mut state = ViewerState::new(false);
        assert!(!state.has_document());
        state.begin_opening("Field notes".into(), 42, 17, Some(Image::default()));
        // Everything the viewer needs to switch screens, before any render.
        assert!(state.has_document());
        assert_eq!(state.title(), "Field notes");
        assert_eq!(state.total(), 42);
        // It opens where the tablet left off, not on the first page.
        assert_eq!(state.page(), 17);
        assert_eq!(state.page_field, "18");
        assert!(
            state.visible_overview().is_some(),
            "the library cover stands in for the page"
        );
        assert!(state.showing_stale_page());
        // A document with no cached cover still opens, just without an image.
        let mut bare = ViewerState::new(false);
        bare.begin_opening("Sketches".into(), 1, 0, None);
        assert!(bare.has_document());
        assert!(bare.visible_overview().is_none());
        assert!(!bare.showing_stale_page());
        // The first raster replaces the placeholder.
        state.set_preview(preview(1404.0, 1872.0), Some(Image::default()));
        assert_eq!(state.title(), "Notes");
        assert_eq!(state.total(), 3);
        assert!(!state.showing_stale_page());
    }

    #[test]
    fn notes_and_warnings_are_split_by_severity() {
        let mut state = state(1404.0, 1872.0);
        let preview = state.preview.as_mut().expect("preview");
        preview.warnings = vec![
            "a content problem".into(),
            format!("{}an approximation", notebook::NOTE_PREFIX),
        ];
        assert_eq!(state.warnings(), vec!["a content problem".to_string()]);
        // The marker is stripped before the note reaches the UI.
        assert_eq!(state.notes(), vec!["an approximation".to_string()]);
        assert!(ViewerState::new(false).warnings().is_empty());
        assert!(ViewerState::new(false).notes().is_empty());
    }

    #[test]
    fn the_rasterized_export_caveat_is_not_a_page_note() {
        let state = state(1404.0, 1872.0);
        assert!(state
            .warnings()
            .iter()
            .chain(state.notes().iter())
            .all(|entry| !entry.contains("rasterized")));
    }
}
