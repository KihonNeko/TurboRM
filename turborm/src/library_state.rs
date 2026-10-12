use slint::Image;
use std::collections::{HashMap, HashSet, VecDeque};
use turborm_core::{folders, library::Entry};

const COVER_CACHE_LIMIT: usize = 128;

/// What the sidebar rail is filtering on. `Folder` browses the hierarchy;
/// every other value is a flat filter across the whole library.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Scope {
    Folder,
    Pinned,
    Notebooks,
    Pdfs,
    Ebooks,
}

impl Scope {
    /// The identifier exchanged with the sidebar. Presentation never owns it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Folder => "",
            Self::Pinned => "pinned",
            Self::Notebooks => "notebook",
            Self::Pdfs => "pdf",
            Self::Ebooks => "epub",
        }
    }
    pub fn from_str(value: &str) -> Self {
        match value {
            "pinned" => Self::Pinned,
            "notebook" => Self::Notebooks,
            "pdf" => Self::Pdfs,
            "epub" => Self::Ebooks,
            _ => Self::Folder,
        }
    }
    /// The `.content` file type this scope keeps, if it filters on one.
    fn file_type(self) -> Option<&'static str> {
        match self {
            Self::Notebooks => Some("notebook"),
            Self::Pdfs => Some("pdf"),
            Self::Ebooks => Some("epub"),
            _ => None,
        }
    }
    /// The heading shown for this scope at the library root.
    pub fn heading(self) -> &'static str {
        match self {
            Self::Folder => "My files",
            Self::Pinned => "Favorites",
            Self::Notebooks => "Notebooks",
            Self::Pdfs => "PDFs",
            Self::Ebooks => "Ebooks",
        }
    }
}

/// How the current view is ordered. Rust owns it; Slint only displays it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Sort {
    Modified,
    Name,
}

impl Sort {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Modified => "modified",
            Self::Name => "name",
        }
    }
    pub fn from_str(value: &str) -> Self {
        if value == "name" {
            Self::Name
        } else {
            Self::Modified
        }
    }
}

/// The "New folder" dialog. Rust owns the typed name and its message; Slint
/// only presents them.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct FolderDialog {
    /// Where the folder goes: "" for the library root, else a folder ID.
    pub parent: String,
    pub name: String,
    /// The inline validation message; empty while the name is acceptable.
    pub error: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum View {
    Covers,
    List,
}

pub struct LibraryState {
    pub entries: Vec<Entry>,
    pub folder: String,
    pub scope: Scope,
    pub sort: Sort,
    pub view: View,
    pub search: String,
    pub selected: Option<String>,
    pub move_source: Option<String>,
    pub move_visible: bool,
    pub folder_dialog: Option<FolderDialog>,
    pub paired: Option<bool>,
    pub busy: bool,
    pub error: String,
    pub status: String,
    pub demo: bool,
    pub epoch: u64,
    index: HashMap<String, usize>,
    /// IDs of folders that hold at least one entry, so folder icons can show
    /// whether they are empty without scanning the library per row.
    occupied: HashSet<String>,
    // Presentation cache only; entry/filter/navigation state never depends on it.
    covers: HashMap<String, Image>,
    cover_order: VecDeque<String>,
    cover_failed: HashSet<String>,
    pending_cover: Option<String>,
    queued_covers: VecDeque<String>,
    queued_cover_ids: HashSet<String>,
    evicted_cover: Option<String>,
}

impl LibraryState {
    pub fn new(demo: bool) -> Self {
        Self {
            entries: Vec::new(),
            folder: String::new(),
            scope: Scope::Folder,
            sort: Sort::Modified,
            view: View::Covers,
            search: String::new(),
            selected: None,
            move_source: None,
            move_visible: false,
            folder_dialog: None,
            paired: None,
            busy: false,
            error: String::new(),
            status: String::new(),
            demo,
            epoch: 0,
            index: HashMap::new(),
            occupied: HashSet::new(),
            covers: HashMap::new(),
            cover_order: VecDeque::new(),
            cover_failed: HashSet::new(),
            pending_cover: None,
            queued_covers: VecDeque::new(),
            queued_cover_ids: HashSet::new(),
            evicted_cover: None,
        }
    }

    pub fn replace_entries(&mut self, entries: Vec<Entry>) {
        self.entries = entries;
        self.index = self
            .entries
            .iter()
            .enumerate()
            .map(|(index, entry)| (entry.id.clone(), index))
            .collect();
        self.occupied = self
            .entries
            .iter()
            .filter(|entry| !entry.parent.is_empty())
            .map(|entry| entry.parent.clone())
            .collect();
        self.epoch = self.epoch.wrapping_add(1);
        self.covers.clear();
        self.cover_order.clear();
        self.cover_failed.clear();
        self.pending_cover = None;
        self.queued_covers.clear();
        self.queued_cover_ids.clear();
        self.evicted_cover = None;
        if !self.folder.is_empty() && self.entry(&self.folder).is_none_or(|e| e.kind != "folder") {
            self.folder.clear();
        }
        if self
            .selected
            .as_deref()
            .is_some_and(|id| self.entry(id).is_none())
        {
            self.selected = None;
        }
    }

    pub fn entry(&self, id: &str) -> Option<&Entry> {
        self.index
            .get(id)
            .and_then(|&index| self.entries.get(index))
    }

    /// The indices of the entries the current scope, search and sort select.
    /// Folders sort ahead of documents in both orders, matching the reference
    /// client's grouped layout.
    pub fn visible(&self) -> Vec<usize> {
        let query = self.search.to_lowercase();
        let wanted = self.scope.file_type();
        let mut result = self
            .entries
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| {
                let in_scope = match self.scope {
                    Scope::Folder => entry.parent == self.folder,
                    Scope::Pinned => entry.pinned,
                    // A type filter is flat across the library and applies to
                    // documents only; a folder has no `.content` file type.
                    Scope::Notebooks | Scope::Pdfs | Scope::Ebooks => {
                        entry.file_type.as_deref() == wanted
                    }
                };
                (in_scope && (query.is_empty() || entry.title.to_lowercase().contains(&query)))
                    .then_some(index)
            })
            .collect::<Vec<_>>();
        result.sort_by(|&a, &b| {
            let (left, right) = (&self.entries[a], &self.entries[b]);
            let folder_first = (right.kind == "folder").cmp(&(left.kind == "folder"));
            folder_first.then_with(|| match self.sort {
                // `lastModified` is epoch milliseconds as a string, so a
                // numeric comparison is used and unparseable values sort last.
                Sort::Modified => modified_key(right)
                    .cmp(&modified_key(left))
                    .then_with(|| left.title.to_lowercase().cmp(&right.title.to_lowercase())),
                Sort::Name => left
                    .title
                    .to_lowercase()
                    .cmp(&right.title.to_lowercase())
                    .then_with(|| left.id.cmp(&right.id)),
            })
        });
        result
    }

    pub fn navigate(&mut self, id: String) {
        if id.is_empty() || self.entry(&id).is_some_and(|e| e.kind == "folder") {
            self.folder = id;
            self.scope = Scope::Folder;
            self.selected = None;
            self.search.clear();
        }
    }

    pub fn go_back(&mut self) {
        self.folder = self
            .entry(&self.folder)
            .map(|e| e.parent.clone())
            .unwrap_or_default();
        self.scope = Scope::Folder;
        self.selected = None;
    }

    pub fn breadcrumbs(&self) -> Vec<(String, String)> {
        let mut result = vec![(String::new(), "My files".into())];
        if self.scope != Scope::Folder {
            return vec![(String::new(), self.scope.heading().into())];
        }
        let mut chain = Vec::new();
        let mut current = self.folder.as_str();
        let mut seen = HashSet::new();
        while !current.is_empty() && seen.insert(current.to_owned()) {
            let Some(entry) = self.entry(current) else {
                break;
            };
            chain.push((entry.id.clone(), entry.title.clone()));
            current = &entry.parent;
        }
        chain.reverse();
        result.extend(chain);
        result
    }

    pub fn move_targets(&self) -> Vec<(String, String)> {
        let source = self.move_source.as_deref().unwrap_or_default();
        let mut children = HashMap::<&str, Vec<&str>>::new();
        for entry in &self.entries {
            children
                .entry(entry.parent.as_str())
                .or_default()
                .push(entry.id.as_str());
        }
        let mut forbidden = HashSet::new();
        let mut pending = vec![source];
        while let Some(parent) = pending.pop() {
            if !forbidden.insert(parent) {
                continue;
            }
            if let Some(descendants) = children.get(parent) {
                pending.extend(descendants.iter().copied());
            }
        }
        let mut result = self
            .entries
            .iter()
            .filter(|e| e.kind == "folder" && !forbidden.contains(e.id.as_str()))
            .map(|e| {
                (
                    e.id.clone(),
                    if e.path.is_empty() {
                        e.title.clone()
                    } else {
                        e.path.clone()
                    },
                )
            })
            .collect::<Vec<_>>();
        result.sort_by_key(|(_, title)| title.to_lowercase());
        result.insert(0, (String::new(), "Library root".into()));
        result
    }

    /// The folder that dropping entry `id` on `folder` moves it into, or
    /// `None` when that drop would do nothing or is not allowed. `"."` is the
    /// folder being browsed, and `""` the library root.
    pub fn drop_target(&self, id: &str, folder: &str) -> Option<String> {
        if !self.can_drag() {
            return None;
        }
        // A flat filter's only breadcrumb is its heading, also with id "".
        if self.scope != Scope::Folder && matches!(folder, "." | "") {
            return None;
        }
        let folder = if folder == "." {
            self.folder.as_str()
        } else {
            folder
        };
        let entry = self.entry(id)?;
        if entry.parent == folder {
            return None;
        }
        // Into the folder itself or anything inside it would orphan it.
        let mut current = folder;
        let mut seen = HashSet::new();
        while !current.is_empty() && seen.insert(current) {
            let ancestor = self.entry(current)?;
            if ancestor.kind != "folder" || ancestor.id == id {
                return None;
            }
            current = &ancestor.parent;
        }
        Some(folder.to_owned())
    }

    /// Whether entries may be dragged at all: connected, not in the demo,
    /// and with no operation running.
    pub fn can_drag(&self) -> bool {
        !self.demo && !self.busy && self.paired == Some(true)
    }

    /// Whether "New folder" applies to the current view: browsing "My files"
    /// (not a flat filter), paired and connected to the cloud. The toolbar
    /// also greys it out while an operation runs.
    pub fn can_create_folder(&self) -> bool {
        self.scope == Scope::Folder && !self.demo && self.paired == Some(true)
    }

    /// Whether "New folder inside…" applies to the selected entry.
    pub fn can_create_inside(&self) -> bool {
        !self.demo
            && self.paired == Some(true)
            && self
                .selected
                .as_deref()
                .and_then(|id| self.entry(id))
                .is_some_and(|entry| entry.kind == "folder")
    }

    /// Opens the dialog for a new folder under `parent` ("" is the root).
    /// Returns false, and leaves the state alone, when the parent is not a
    /// known folder or another operation is running.
    pub fn open_folder_dialog(&mut self, parent: String) -> bool {
        if self.busy
            || !(parent.is_empty() || self.entry(&parent).is_some_and(|e| e.kind == "folder"))
        {
            return false;
        }
        self.move_visible = false;
        self.move_source = None;
        self.folder_dialog = Some(FolderDialog {
            parent,
            ..FolderDialog::default()
        });
        true
    }

    /// The title of the dialog's destination, for its "In …" line.
    pub fn folder_dialog_location(&self) -> String {
        match self.folder_dialog.as_ref() {
            Some(dialog) if !dialog.parent.is_empty() => self
                .entry(&dialog.parent)
                .map_or_else(|| Scope::Folder.heading().into(), |e| e.title.clone()),
            _ => Scope::Folder.heading().into(),
        }
    }

    /// Why `name` cannot be a new folder under `parent`, or `None` when it
    /// can. The cloud repeats every check before it creates anything.
    pub fn folder_name_error(&self, parent: &str, name: &str) -> Option<String> {
        if let Err(error) = folders::validate_name(name) {
            return Some(error.to_string());
        }
        folders::find_child_folder(&self.entries, parent, name).map(|existing| {
            format!(
                "A folder named \u{201c}{}\u{201d} already exists here",
                existing.title
            )
        })
    }

    /// Records a keystroke. An empty field is not reported until Create, so
    /// the dialog does not open with an error.
    pub fn edit_folder_name(&mut self, name: String) {
        let error = if name.trim().is_empty() {
            None
        } else {
            let parent = self.folder_dialog.as_ref().map(|d| d.parent.clone());
            parent.and_then(|parent| self.folder_name_error(&parent, &name))
        };
        if let Some(dialog) = self.folder_dialog.as_mut() {
            dialog.name = name;
            dialog.error = error.unwrap_or_default();
        }
    }

    /// Create was pressed. Returns the parent and trimmed name and closes the
    /// dialog when the name is acceptable; otherwise shows the message.
    pub fn confirm_folder_dialog(&mut self) -> Option<(String, String)> {
        let dialog = self.folder_dialog.as_ref()?;
        if let Some(error) = self.folder_name_error(&dialog.parent, &dialog.name) {
            if let Some(dialog) = self.folder_dialog.as_mut() {
                dialog.error = error;
            }
            return None;
        }
        let dialog = self.folder_dialog.take()?;
        Some((dialog.parent, dialog.name.trim().to_owned()))
    }

    /// Offline demo only: adds the folder to the in-memory library, selects
    /// it and returns its synthetic ID. Nothing leaves the process.
    pub fn create_demo_folder(&mut self, parent: String, name: String) -> String {
        let mut serial = self.entries.len();
        let id = loop {
            let id = format!("demo-folder-{serial}");
            if self.entry(&id).is_none() {
                break id;
            }
            serial += 1;
        };
        let modified = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |time| time.as_millis());
        let mut entries = std::mem::take(&mut self.entries);
        entries.push(Entry {
            id: id.clone(),
            title: name,
            kind: "folder".into(),
            parent,
            deleted: false,
            pinned: false,
            modified: Some(modified.to_string()),
            files: 1,
            size: 0,
            path: String::new(),
            hierarchy_warning: None,
            file_type: None,
            page_count: None,
        });
        self.replace_entries(entries);
        self.selected = Some(id.clone());
        id
    }

    /// Whether the folder `id` contains any document or subfolder.
    pub fn has_content(&self, id: &str) -> bool {
        self.occupied.contains(id)
    }

    pub fn cover(&self, id: &str) -> Option<&Image> {
        self.covers.get(id)
    }
    pub fn cover_failed(&self, id: &str) -> bool {
        self.cover_failed.contains(id)
    }
    pub fn request_cover(&mut self, id: &str) -> Option<(String, u64)> {
        if self.demo
            || self.covers.contains_key(id)
            || self.cover_failed.contains(id)
            || self.pending_cover.as_deref() == Some(id)
            || self.queued_cover_ids.contains(id)
            || self.entry(id).is_none_or(|e| e.kind != "document")
        {
            return None;
        }
        if self.pending_cover.is_some() {
            if self.queued_covers.len() < COVER_CACHE_LIMIT {
                self.queued_covers.push_back(id.to_owned());
                self.queued_cover_ids.insert(id.to_owned());
            }
            None
        } else {
            self.pending_cover = Some(id.to_owned());
            Some((id.to_owned(), self.epoch))
        }
    }
    pub fn finish_cover(&mut self, id: String, epoch: u64, image: Result<Image, ()>) -> bool {
        if epoch != self.epoch || self.entry(&id).is_none() {
            return false;
        }
        if self.pending_cover.as_deref() == Some(&id) {
            self.pending_cover = None;
        }
        match image {
            Ok(image) => {
                if !self.covers.contains_key(&id) {
                    self.cover_order.push_back(id.clone());
                }
                self.cover_failed.remove(&id);
                self.covers.insert(id, image);
                while self.cover_order.len() > COVER_CACHE_LIMIT {
                    if let Some(old) = self.cover_order.pop_front() {
                        self.covers.remove(&old);
                        self.evicted_cover = Some(old);
                    }
                }
            }
            Err(()) => {
                self.cover_failed.insert(id);
            }
        }
        true
    }

    pub fn next_cover_request(&mut self) -> Option<(String, u64)> {
        if self.pending_cover.is_some() {
            return None;
        }
        while let Some(id) = self.queued_covers.pop_front() {
            self.queued_cover_ids.remove(&id);
            if self
                .entry(&id)
                .is_some_and(|entry| entry.kind == "document")
            {
                self.pending_cover = Some(id.clone());
                return Some((id, self.epoch));
            }
        }
        None
    }

    pub fn clear_queued_covers(&mut self) {
        self.queued_covers.clear();
        self.queued_cover_ids.clear();
    }

    pub fn take_evicted_cover(&mut self) -> Option<String> {
        self.evicted_cover.take()
    }
}

/// Sort key for "recently modified": the epoch milliseconds, or zero when the
/// cloud value is absent or not numeric, so those entries sort last.
fn modified_key(entry: &Entry) -> u64 {
    entry
        .modified
        .as_deref()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn entry(id: &str, parent: &str, kind: &str, pinned: bool) -> Entry {
        Entry {
            id: id.into(),
            title: id.into(),
            kind: kind.into(),
            parent: parent.into(),
            deleted: false,
            pinned,
            modified: None,
            files: 0,
            size: 0,
            path: id.into(),
            hierarchy_warning: None,
            file_type: None,
            page_count: None,
        }
    }
    fn typed(id: &str, file_type: &str) -> Entry {
        let mut e = entry(id, "", "document", false);
        e.file_type = Some(file_type.into());
        e
    }
    #[test]
    fn navigation_search_pinned_and_selection() {
        let mut s = LibraryState::new(false);
        s.replace_entries(vec![
            entry("f", "", "folder", false),
            entry("a", "f", "document", true),
            entry("b", "", "document", true),
        ]);
        s.navigate("f".into());
        assert_eq!(s.visible(), vec![1]);
        s.search = "z".into();
        assert!(s.visible().is_empty());
        s.scope = Scope::Pinned;
        s.search.clear();
        // Name order is used so the assertion does not depend on the modified
        // field, which the helper leaves unset.
        s.sort = Sort::Name;
        assert_eq!(s.visible(), vec![1, 2]);
        s.selected = Some("a".into());
        s.go_back();
        assert!(s.selected.is_none());
    }
    #[test]
    fn folders_know_whether_they_hold_anything() {
        let mut s = LibraryState::new(false);
        s.replace_entries(vec![
            entry("full", "", "folder", false),
            entry("nested", "full", "folder", false),
            entry("empty", "", "folder", false),
            entry("doc", "nested", "document", false),
        ]);
        assert!(s.has_content("full"));
        assert!(s.has_content("nested"));
        assert!(!s.has_content("empty"));
        let id = s.create_demo_folder("empty".into(), "New".into());
        assert!(s.has_content("empty"));
        assert!(!s.has_content(&id));
    }
    #[test]
    fn file_type_scopes_select_only_that_type() {
        let mut s = LibraryState::new(false);
        s.replace_entries(vec![
            entry("f", "", "folder", false),
            typed("note", "notebook"),
            typed("book", "epub"),
            typed("paper", "pdf"),
            entry("unknown", "", "document", false),
        ]);
        for (scope, id) in [
            (Scope::Notebooks, "note"),
            (Scope::Pdfs, "paper"),
            (Scope::Ebooks, "book"),
        ] {
            s.scope = scope;
            let ids = s
                .visible()
                .into_iter()
                .map(|i| s.entries[i].id.clone())
                .collect::<Vec<_>>();
            assert_eq!(ids, vec![id.to_string()], "scope {scope:?}");
        }
        // A document whose `.content` has not been read yet is not claimed by
        // any type filter, and folders never are.
        s.scope = Scope::Folder;
        assert_eq!(s.visible().len(), 5);
        assert_eq!(Scope::from_str("pdf"), Scope::Pdfs);
        assert_eq!(Scope::Ebooks.as_str(), "epub");
        assert_eq!(Scope::from_str("nonsense"), Scope::Folder);
    }
    #[test]
    fn sort_orders_folders_first_then_by_key() {
        let mut s = LibraryState::new(false);
        let modified = |id: &str, kind: &str, millis: &str| {
            let mut e = entry(id, "", kind, false);
            e.modified = Some(millis.into());
            e
        };
        s.replace_entries(vec![
            modified("beta", "document", "2000"),
            modified("alpha", "document", "3000"),
            modified("zeta", "folder", "1000"),
            modified("gamma", "document", "bad value"),
        ]);
        let ids = |s: &LibraryState| {
            s.visible()
                .into_iter()
                .map(|i| s.entries[i].id.clone())
                .collect::<Vec<_>>()
        };
        s.sort = Sort::Modified;
        assert_eq!(ids(&s), vec!["zeta", "alpha", "beta", "gamma"]);
        s.sort = Sort::Name;
        assert_eq!(ids(&s), vec!["zeta", "alpha", "beta", "gamma"]);
        assert_eq!(Sort::from_str("name"), Sort::Name);
        assert_eq!(Sort::from_str("anything"), Sort::Modified);
        assert_eq!(Sort::Modified.as_str(), "modified");
    }
    #[test]
    fn move_excludes_self_and_descendants() {
        let mut s = LibraryState::new(false);
        s.replace_entries(vec![
            entry("a", "", "folder", false),
            entry("b", "a", "folder", false),
            entry("c", "", "folder", false),
        ]);
        s.move_source = Some("a".into());
        let ids = s
            .move_targets()
            .into_iter()
            .map(|x| x.0)
            .collect::<Vec<_>>();
        assert_eq!(ids, vec!["", "c"]);
    }
    #[test]
    fn drops_go_only_where_a_move_changes_something() {
        let mut s = LibraryState::new(false);
        s.paired = Some(true);
        s.replace_entries(vec![
            entry("a", "", "folder", false),
            entry("b", "a", "folder", false),
            entry("c", "", "folder", false),
            entry("note", "a", "document", false),
            entry("pdf", "", "document", false),
        ]);
        assert_eq!(s.drop_target("note", "c").as_deref(), Some("c"));
        assert_eq!(s.drop_target("note", "").as_deref(), Some(""));
        assert_eq!(s.drop_target("b", "c").as_deref(), Some("c"));
        // Already there, onto a document, or into itself or its own subfolder.
        assert_eq!(s.drop_target("note", "a"), None);
        assert_eq!(s.drop_target("note", "pdf"), None);
        assert_eq!(s.drop_target("a", "a"), None);
        assert_eq!(s.drop_target("a", "b"), None);
        assert_eq!(s.drop_target("gone", "c"), None);
        // "." is the folder being browsed, and only while browsing folders.
        s.navigate("c".into());
        assert_eq!(s.drop_target("note", ".").as_deref(), Some("c"));
        s.navigate("a".into());
        assert_eq!(s.drop_target("note", "."), None);
        s.scope = Scope::Pinned;
        assert_eq!(s.drop_target("pdf", "."), None);
        s.scope = Scope::Folder;
        s.busy = true;
        assert_eq!(s.drop_target("note", "c"), None);
    }
    #[test]
    fn stale_thumbnail_event_is_ignored() {
        let mut s = LibraryState::new(false);
        s.replace_entries(vec![entry("a", "", "document", false)]);
        let old = s.epoch;
        s.replace_entries(vec![entry("a", "", "document", false)]);
        assert!(!s.finish_cover("a".into(), old, Err(())));
        assert!(!s.cover_failed("a"));
    }
    #[test]
    fn visible_cover_requests_are_serialized() {
        let mut state = LibraryState::new(false);
        state.replace_entries(vec![
            entry("a", "", "document", false),
            entry("b", "", "document", false),
        ]);
        let first = state.request_cover("a").unwrap();
        assert!(state.request_cover("b").is_none());
        assert!(state.finish_cover(first.0, first.1, Err(())));
        assert_eq!(state.next_cover_request().unwrap().0, "b");
    }
    #[test]
    fn fifty_thousand_projection_is_bounded() {
        let mut s = LibraryState::new(true);
        s.replace_entries(
            (0..50_000)
                .map(|i| entry(&format!("d{i}"), "", "document", i % 2 == 0))
                .collect(),
        );
        s.scope = Scope::Pinned;
        assert_eq!(s.visible().len(), 25_000);
    }
}
