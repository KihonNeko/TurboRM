use super::*;
use crate::manifest::{Manifest, Row};
use anyhow::{bail, ensure};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Mutex;

const FOLDER: &str = "22222222-2222-4222-8222-222222222222";
const CHILD: &str = "33333333-3333-4333-8333-333333333333";
const DOCUMENT: &str = "44444444-4444-4444-8444-444444444444";
const DELETED: &str = "55555555-5555-4555-8555-555555555555";
const TRASHED: &str = "66666666-6666-4666-8666-666666666666";
const ABSENT: &str = "88888888-8888-4888-8888-888888888888";

struct State {
    root_hash: String,
    generation: u64,
    blobs: HashMap<String, Vec<u8>>,
    writes: Vec<String>,
    /// Answer this many root publications with HTTP 412, each time letting
    /// "another client" add a document first.
    conflicts: usize,
    fail_publish: bool,
}
struct Mock(Mutex<State>);

impl Mock {
    fn publishes(&self) -> usize {
        let state = self.0.lock().unwrap();
        state.writes.iter().filter(|w| *w == "publish").count()
    }
    fn writes(&self) -> usize {
        self.0.lock().unwrap().writes.len()
    }
    fn root(&self) -> Manifest {
        let state = self.0.lock().unwrap();
        Manifest::parse(state.blobs.get(&state.root_hash).unwrap()).unwrap()
    }
    fn metadata(&self, id: &str) -> (Manifest, Vec<u8>) {
        let root = self.root();
        let state = self.0.lock().unwrap();
        let row = root.rows.iter().find(|row| row.name == id).unwrap();
        let document = Manifest::parse(state.blobs.get(&row.hash).unwrap()).unwrap();
        let bytes = state.blobs.get(&document.rows[0].hash).unwrap().clone();
        (document, bytes)
    }
}

impl Transport for Mock {
    fn request(
        &self,
        _host: &str,
        method: &str,
        path: &str,
        _token: Option<&str>,
        body: Option<Vec<u8>>,
        _headers: &[(&str, String)],
        _limit: usize,
    ) -> Result<Vec<u8>> {
        let mut state = self.0.lock().unwrap();
        if method == "GET" && path == "/sync/v4/root" {
            return Ok(serde_json::to_vec(&json!({
                "schemaVersion": 4,
                "generation": state.generation,
                "hash": state.root_hash,
                "hasMissingHashes": false
            }))?);
        }
        if let Some(hash) = path.strip_prefix("/sync/v3/files/") {
            return match method {
                "GET" => state
                    .blobs
                    .get(hash)
                    .cloned()
                    .ok_or_else(|| anyhow!("missing blob")),
                "PUT" => {
                    let bytes = body.ok_or_else(|| anyhow!("missing body"))?;
                    ensure!(job::hash(&bytes) == hash, "bad blob address");
                    state.writes.push(format!("blob:{hash}"));
                    state.blobs.insert(hash.to_owned(), bytes);
                    Ok(Vec::new())
                }
                _ => bail!("unexpected method"),
            };
        }
        if method == "PUT" && path == "/sync/v3/root" {
            state.writes.push("publish".into());
            if state.fail_publish {
                bail!("simulated uncertain publication failure");
            }
            if state.conflicts > 0 {
                state.conflicts -= 1;
                // Another client publishes a new document in the meantime.
                let mut root = Manifest::parse(state.blobs.get(&state.root_hash).unwrap()).unwrap();
                let serial = state.generation;
                let id = format!("77777777-7777-4777-8777-{serial:012}");
                let row = add_node(&mut state.blobs, &id, "Other", "DocumentType", "", false);
                root.rows.push(row);
                let bytes = root.serialize().unwrap();
                let hash = job::hash(&bytes);
                state.blobs.insert(hash.clone(), bytes);
                state.root_hash = hash;
                state.generation += 1;
                return Err(HttpError {
                    status: 412,
                    route: "/sync/v3/root".into(),
                }
                .into());
            }
            let value: Value = serde_json::from_slice(&body.unwrap())?;
            ensure!(
                value["generation"] == state.generation,
                "unguarded publication"
            );
            let hash = value["hash"].as_str().unwrap().to_owned();
            ensure!(state.blobs.contains_key(&hash), "root blob absent");
            state.root_hash = hash;
            state.generation += 1;
            return Ok(serde_json::to_vec(&json!({"generation":state.generation}))?);
        }
        bail!("unexpected request {method} {path}")
    }
}

fn add_node(
    blobs: &mut HashMap<String, Vec<u8>>,
    id: &str,
    title: &str,
    kind: &str,
    parent: &str,
    deleted: bool,
) -> Row {
    let metadata = serde_json::to_vec(&json!({
        "visibleName": title,
        "type": kind,
        "parent": parent,
        "deleted": deleted,
        "lastModified": "1000",
    }))
    .unwrap();
    let metadata_hash = job::hash(&metadata);
    blobs.insert(metadata_hash.clone(), metadata.clone());
    let content = b"{}".to_vec();
    let content_hash = job::hash(&content);
    blobs.insert(content_hash.clone(), content.clone());
    let document = Manifest {
        name: id.into(),
        flags: 0x40,
        rows: vec![
            Row {
                hash: metadata_hash,
                flags: 0x20,
                name: format!("{id}.metadata"),
                count: 0,
                size: metadata.len() as u64,
            },
            Row {
                hash: content_hash,
                flags: 0,
                name: format!("{id}.content"),
                count: 0,
                size: content.len() as u64,
            },
        ],
    };
    let bytes = document.serialize().unwrap();
    let hash = job::hash(&bytes);
    blobs.insert(hash.clone(), bytes);
    Row {
        hash,
        // Unusual flags prove existing rows are carried over untouched.
        flags: 0x80,
        name: id.into(),
        count: document.rows.len() as u32,
        size: document.total_size().unwrap(),
    }
}

fn fixture() -> Mock {
    let mut blobs = HashMap::new();
    let rows = vec![
        add_node(
            &mut blobs,
            FOLDER,
            "Claude Review",
            "CollectionType",
            "",
            false,
        ),
        add_node(
            &mut blobs,
            CHILD,
            "TurboRM",
            "CollectionType",
            FOLDER,
            false,
        ),
        add_node(&mut blobs, DOCUMENT, "Notes", "DocumentType", "", false),
        add_node(&mut blobs, DELETED, "Old", "CollectionType", "", true),
        add_node(
            &mut blobs,
            TRASHED,
            "Binned",
            "CollectionType",
            "trash",
            false,
        ),
    ];
    let root = Manifest {
        name: ".".into(),
        flags: 0x100,
        rows,
    };
    let bytes = root.serialize().unwrap();
    let root_hash = job::hash(&bytes);
    blobs.insert(root_hash.clone(), bytes);
    Mock(Mutex::new(State {
        root_hash,
        generation: 7,
        blobs,
        writes: Vec::new(),
        conflicts: 0,
        fail_publish: false,
    }))
}

fn create(mock: &Mock, parent: &str, name: &str) -> Result<String> {
    create_folder(mock, "cloud", "token", None, parent, name)
}

fn ensure_path(mock: &Mock, parent: &str, path: &str) -> Result<Vec<FolderStep>> {
    ensure_folder_path(mock, "cloud", "token", None, parent, path)
}

fn assert_only_added(before: &Manifest, after: &Manifest, added: &[&str]) {
    assert_eq!(after.flags, before.flags);
    for row in &before.rows {
        assert_eq!(
            after.rows.iter().find(|r| r.name == row.name),
            Some(row),
            "existing row {} changed",
            row.name
        );
    }
    let mut new: Vec<_> = after
        .rows
        .iter()
        .filter(|r| !before.rows.iter().any(|b| b.name == r.name))
        .map(|r| r.name.as_str())
        .collect();
    let mut added = added.to_vec();
    new.sort_unstable();
    added.sort_unstable();
    assert_eq!(new, added);
}

#[test]
fn creates_a_folder_that_only_adds_one_metadata_entry() {
    let mock = fixture();
    let before = mock.root();
    let id = create(&mock, "", "  Projects ").unwrap();
    let parsed = Uuid::parse_str(&id).unwrap();
    assert_eq!(parsed.get_version_num(), 4);
    assert_eq!(parsed.to_string(), id);
    assert_eq!(mock.publishes(), 1);
    assert_eq!(mock.0.lock().unwrap().generation, 8);
    let after = mock.root();
    assert_only_added(&before, &after, &[&id]);
    let row = after.rows.iter().find(|r| r.name == id).unwrap();
    assert_eq!((row.flags, row.count), (0, 1));

    let (document, bytes) = mock.metadata(&id);
    assert_eq!(document.flags, 0);
    assert_eq!(document.rows.len(), 1);
    assert_eq!(document.rows[0].name, format!("{id}.metadata"));
    assert_eq!(document.rows[0].flags, 0);
    let metadata: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(metadata["type"], "CollectionType");
    assert_eq!(metadata["visibleName"], "Projects", "the name is trimmed");
    assert_eq!(metadata["parent"], "");
    assert_eq!(metadata["pinned"], false);
    assert_eq!(metadata["new"], false);
    assert_eq!(metadata["source"], "");
    let modified = metadata["lastModified"].as_str().unwrap();
    assert!(modified.parse::<u64>().unwrap() > 1_700_000_000_000);
    assert_eq!(metadata["createdTime"], modified);
    assert_eq!(metadata.as_object().unwrap().len(), 8);
}

#[test]
fn creates_inside_an_active_folder() {
    let mock = fixture();
    let before = mock.root();
    let id = create(&mock, FOLDER, "Inbox").unwrap();
    assert_only_added(&before, &mock.root(), &[&id]);
    let metadata: Value = serde_json::from_slice(&mock.metadata(&id).1).unwrap();
    assert_eq!(metadata["parent"], FOLDER);
}

#[test]
fn metadata_matches_a_tablet_created_folder_byte_for_byte() {
    // The "Claude Review" folder the Paper Pro created on 2026-09-23: a single
    // 217-byte .metadata component with this SHA-256.
    let bytes = folder_metadata("Claude Review", "", "1790156306836", "1790156306830");
    assert_eq!(bytes.len(), 217);
    assert_eq!(
        job::hash(&bytes),
        "692f8f9ba6ff050e6fe80cb29cda587a22e29561d3578b7e10c9d7be914c68ab"
    );
    // Quotes and non-ASCII names stay valid JSON.
    let odd = folder_metadata("Algèbre \"2\"", FOLDER, "1", "1");
    let value: Value = serde_json::from_slice(&odd).unwrap();
    assert_eq!(value["visibleName"], "Algèbre \"2\"");
}

#[test]
fn a_duplicate_name_is_refused_ignoring_case_and_spaces() {
    let mock = fixture();
    for name in ["claude review", " CLAUDE REVIEW "] {
        let error = create(&mock, "", name).unwrap_err();
        let exists = error.downcast_ref::<FolderExists>().expect("FolderExists");
        assert_eq!(exists.id, FOLDER);
    }
    let error = create(&mock, FOLDER, "turborm").unwrap_err();
    assert_eq!(error.downcast_ref::<FolderExists>().unwrap().id, CHILD);
    assert_eq!(mock.writes(), 0, "a refused duplicate writes nothing");
    // A deleted folder or a document with that name does not block creation,
    // and the same name is fine under another parent.
    create(&mock, "", "Old").unwrap();
    create(&mock, "", "Notes").unwrap();
    create(&mock, CHILD, "Claude Review").unwrap();
}

#[test]
fn invalid_names_are_refused_before_any_request() {
    let mock = fixture();
    let long = "x".repeat(MAX_NAME + 1);
    for name in ["", "   ", "a/b", "tab\there", "line\nbreak", long.as_str()] {
        assert!(create(&mock, "", name).is_err(), "{name:?} must be refused");
    }
    assert_eq!(mock.writes(), 0);
    assert_eq!(
        validate_name(&"é".repeat(MAX_NAME))
            .unwrap()
            .chars()
            .count(),
        MAX_NAME
    );
}

#[test]
fn the_parent_must_be_an_active_folder_outside_the_trash() {
    let mock = fixture();
    for parent in [
        ABSENT,
        DELETED,
        DOCUMENT,
        TRASHED,
        "trash",
        "not-a-uuid",
        "22222222-2222-4222-8222-22222222222A",
        "00000000-0000-0000-0000-000000000000",
    ] {
        assert!(create(&mock, parent, "New").is_err(), "parent {parent}");
        assert!(
            ensure_path(&mock, parent, "New").is_err(),
            "parent {parent}"
        );
    }
    assert_eq!(mock.writes(), 0);
}

#[test]
fn a_generation_conflict_retries_from_a_fresh_snapshot() {
    let mock = fixture();
    mock.0.lock().unwrap().conflicts = 1;
    let before = mock.root();
    let id = create(&mock, "", "Retried").unwrap();
    assert_eq!(mock.publishes(), 2);
    let after = mock.root();
    // The other client's document and the new folder are both kept.
    assert_only_added(
        &before,
        &after,
        &["77777777-7777-4777-8777-000000000007", &id],
    );
}

#[test]
fn repeated_conflicts_stop_after_the_attempt_limit() {
    let mock = fixture();
    mock.0.lock().unwrap().conflicts = MAX_ATTEMPTS;
    let error = create(&mock, "", "Busy").unwrap_err();
    assert!(
        error.to_string().contains("no folder was created"),
        "{error}"
    );
    assert_eq!(mock.publishes(), MAX_ATTEMPTS);
    // Only the other client's documents were added; no folder was.
    assert_eq!(mock.root().rows.len(), 5 + MAX_ATTEMPTS);
}

#[test]
fn an_uncertain_publication_is_not_retried() {
    let mock = fixture();
    mock.0.lock().unwrap().fail_publish = true;
    let error = create(&mock, "", "Uncertain").unwrap_err();
    assert!(format!("{error:#}").contains("No automatic retry"));
    assert_eq!(mock.publishes(), 1);
}

#[test]
fn a_nested_path_is_created_and_then_reused() {
    let mock = fixture();
    let before = mock.root();
    let steps = ensure_path(&mock, "", "/Projects/TurboRM/Reviews/").unwrap();
    assert_eq!(steps.len(), 3);
    assert!(steps.iter().all(|step| step.created));
    let ids: Vec<&str> = steps.iter().map(|s| s.id.as_str()).collect();
    assert_only_added(&before, &mock.root(), &ids);
    let parent_of = |id: &str| -> String {
        let metadata: Value = serde_json::from_slice(&mock.metadata(id).1).unwrap();
        metadata["parent"].as_str().unwrap().to_owned()
    };
    assert_eq!(parent_of(ids[0]), "");
    assert_eq!(parent_of(ids[1]), ids[0]);
    assert_eq!(parent_of(ids[2]), ids[1]);

    let writes = mock.writes();
    let again = ensure_path(&mock, "", "projects/turborm/REVIEWS").unwrap();
    assert!(again.iter().all(|step| !step.created));
    assert_eq!(
        again.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
        ids,
        "the second walk reuses every folder"
    );
    assert_eq!(mock.writes(), writes, "reuse writes nothing");
}

#[test]
fn a_path_reuses_the_existing_prefix_and_creates_only_the_rest() {
    let mock = fixture();
    let before = mock.root();
    let steps = ensure_path(&mock, "", "claude review/TurboRM/Round 2").unwrap();
    assert_eq!((steps[0].id.as_str(), steps[0].created), (FOLDER, false));
    assert_eq!((steps[1].id.as_str(), steps[1].created), (CHILD, false));
    assert!(steps[2].created);
    assert_only_added(&before, &mock.root(), &[&steps[2].id]);
    assert_eq!(mock.publishes(), 1);
}

#[test]
fn paths_are_split_and_validated() {
    assert_eq!(split_path("A/B").unwrap(), vec!["A", "B"]);
    assert_eq!(split_path(" /A / B/ ").unwrap(), vec!["A", "B"]);
    assert!(split_path("").is_err());
    assert!(split_path("/").is_err());
    assert!(split_path("A//B").is_err());
    assert!(split_path("A/\u{7}").is_err());
    assert!(split_path(&vec!["a"; MAX_DEPTH + 1].join("/")).is_err());
    assert_eq!(
        split_path(&vec!["a"; MAX_DEPTH].join("/")).unwrap().len(),
        MAX_DEPTH
    );
}
