use super::*;
use crate::manifest::{Manifest, Row};
use anyhow::{bail, ensure};
use std::collections::HashMap;
use std::sync::Mutex;

const ITEM: &str = "11111111-1111-4111-8111-111111111111";
const FOLDER: &str = "22222222-2222-4222-8222-222222222222";
const CHILD: &str = "33333333-3333-4333-8333-333333333333";
const DOCUMENT: &str = "44444444-4444-4444-8444-444444444444";

struct State {
    root_hash: String,
    generation: u64,
    blobs: HashMap<String, Vec<u8>>,
    writes: Vec<String>,
    fail_publish: bool,
}
struct Mock(Mutex<State>);

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
    kind: &str,
    parent: &str,
    deleted: bool,
    extra: Value,
) -> Row {
    let mut metadata = json!({
        "visibleName": id,
        "type": kind,
        "parent": parent,
        "deleted": deleted,
        "unknownFutureField": extra
    });
    // Keep an unrelated nested value to prove a move doesn't reconstruct metadata.
    metadata["nested"] = json!({"a":[1, true, "x"]});
    let metadata = serde_json::to_vec(&metadata).unwrap();
    let metadata_hash = job::hash(&metadata);
    blobs.insert(metadata_hash.clone(), metadata.clone());
    let content = format!("payload-{id}").into_bytes();
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
                flags: 0x10,
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
        flags: 0x80,
        name: id.into(),
        count: document.rows.len() as u32,
        size: document.total_size().unwrap(),
    }
}

fn fixture(item_parent: &str) -> Mock {
    let mut blobs = HashMap::new();
    let rows = vec![
        add_node(
            &mut blobs,
            ITEM,
            "CollectionType",
            item_parent,
            false,
            json!("kept"),
        ),
        add_node(&mut blobs, FOLDER, "CollectionType", "", false, json!(42)),
        add_node(
            &mut blobs,
            CHILD,
            "CollectionType",
            ITEM,
            false,
            json!(null),
        ),
        add_node(
            &mut blobs,
            DOCUMENT,
            "DocumentType",
            "",
            false,
            json!({"v":1}),
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
        fail_publish: false,
    }))
}

fn published_metadata(mock: &Mock, id: &str) -> (Manifest, Manifest, Value) {
    let state = mock.0.lock().unwrap();
    let root = Manifest::parse(state.blobs.get(&state.root_hash).unwrap()).unwrap();
    let row = root.rows.iter().find(|row| row.name == id).unwrap();
    let document = Manifest::parse(state.blobs.get(&row.hash).unwrap()).unwrap();
    let metadata_row = document
        .rows
        .iter()
        .find(|row| row.name == format!("{id}.metadata"))
        .unwrap();
    let metadata = serde_json::from_slice(state.blobs.get(&metadata_row.hash).unwrap()).unwrap();
    (root, document, metadata)
}

#[test]
fn move_preserves_unknown_metadata_components_rows_and_flags() {
    let mock = fixture("");
    move_entry(&mock, "cloud", "token", ITEM, FOLDER).unwrap();
    let state = mock.0.lock().unwrap();
    assert_eq!(state.generation, 8);
    assert_eq!(state.writes.iter().filter(|v| *v == "publish").count(), 1);
    drop(state);
    let (root, document, metadata) = published_metadata(&mock, ITEM);
    assert_eq!(root.flags, 0x100);
    assert_eq!(
        root.rows.iter().find(|r| r.name == ITEM).unwrap().flags,
        0x80
    );
    assert_eq!(document.flags, 0x40);
    assert_eq!(document.rows.len(), 2);
    assert_eq!(
        document
            .rows
            .iter()
            .find(|r| r.name.ends_with(".content"))
            .unwrap()
            .flags,
        0x10
    );
    assert_eq!(
        document
            .rows
            .iter()
            .find(|r| r.name.ends_with(".metadata"))
            .unwrap()
            .flags,
        0x20
    );
    assert_eq!(metadata["parent"], FOLDER);
    assert_eq!(metadata["unknownFutureField"], "kept");
    assert_eq!(metadata["nested"], json!({"a":[1, true, "x"]}));
}

#[test]
fn root_move_uses_empty_parent_and_noop_does_not_write() {
    let mock = fixture(FOLDER);
    move_entry(&mock, "cloud", "token", ITEM, "").unwrap();
    assert_eq!(published_metadata(&mock, ITEM).2["parent"], "");
    let writes = mock.0.lock().unwrap().writes.len();
    move_entry(&mock, "cloud", "token", ITEM, "").unwrap();
    assert_eq!(mock.0.lock().unwrap().writes.len(), writes);
}

#[test]
fn descendant_cycle_is_rejected_before_writes() {
    let mock = fixture("");
    let error = move_entry(&mock, "cloud", "token", ITEM, CHILD).unwrap_err();
    assert!(error.to_string().contains("descendant"));
    assert!(mock.0.lock().unwrap().writes.is_empty());
}

#[test]
fn non_folder_destination_is_rejected_before_writes() {
    let mock = fixture("");
    let error = move_entry(&mock, "cloud", "token", FOLDER, DOCUMENT).unwrap_err();
    assert!(error.to_string().contains("not a folder"));
    assert!(mock.0.lock().unwrap().writes.is_empty());
}

#[test]
fn active_document_can_be_moved() {
    let mock = fixture("");
    move_entry(&mock, "cloud", "token", DOCUMENT, FOLDER).unwrap();
    assert_eq!(published_metadata(&mock, DOCUMENT).2["parent"], FOLDER);
}

#[test]
fn trash_and_invalid_ids_are_rejected_without_network_writes() {
    let mock = fixture("");
    assert!(move_entry(&mock, "cloud", "token", ITEM, "trash").is_err());
    assert!(move_entry(&mock, "cloud", "token", "not-an-id", "").is_err());
    assert!(mock.0.lock().unwrap().writes.is_empty());
}

#[test]
fn deleted_destination_is_rejected_before_writes() {
    let mut blobs = HashMap::new();
    let rows = vec![
        add_node(&mut blobs, ITEM, "DocumentType", "", false, json!(1)),
        add_node(&mut blobs, FOLDER, "CollectionType", "", true, json!(2)),
    ];
    let root = Manifest {
        name: ".".into(),
        flags: 0,
        rows,
    };
    let bytes = root.serialize().unwrap();
    let root_hash = job::hash(&bytes);
    blobs.insert(root_hash.clone(), bytes);
    let mock = Mock(Mutex::new(State {
        root_hash,
        generation: 7,
        blobs,
        writes: Vec::new(),
        fail_publish: false,
    }));
    assert!(move_entry(&mock, "cloud", "token", ITEM, FOLDER).is_err());
    assert!(mock.0.lock().unwrap().writes.is_empty());
}

#[test]
fn publication_failure_is_attempted_once_and_never_retried() {
    let mock = fixture("");
    mock.0.lock().unwrap().fail_publish = true;
    let error = move_entry(&mock, "cloud", "token", ITEM, FOLDER).unwrap_err();
    assert!(error.to_string().contains("No automatic retry"));
    assert_eq!(
        mock.0
            .lock()
            .unwrap()
            .writes
            .iter()
            .filter(|v| *v == "publish")
            .count(),
        1
    );
}
