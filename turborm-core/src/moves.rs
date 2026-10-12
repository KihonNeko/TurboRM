//! Safe, copy-on-write moves within a schema-4 cloud library.
use crate::{
    cloud::{self, Transport, JSON_LIMIT},
    hash_tree, job,
    manifest::{Manifest, Row},
};
use anyhow::{anyhow, ensure, Context, Result};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use uuid::Uuid;

#[derive(Clone)]
struct Node {
    document: Manifest,
    metadata: Value,
    parent: String,
    folder: bool,
}

fn canonical_id(id: &str) -> Result<()> {
    ensure!(
        Uuid::parse_str(id).is_ok_and(|value| !value.is_nil() && value.to_string() == id),
        "Use a canonical document/folder UUID"
    );
    Ok(())
}

fn root_row<'a>(root: &'a Manifest, id: &str) -> Result<&'a Row> {
    root.rows
        .iter()
        .find(|row| row.name == id)
        .ok_or_else(|| anyhow!("Document/folder is not present in this cloud snapshot"))
}

fn load_node(
    http: &impl Transport,
    host: &str,
    token: &str,
    root: &Manifest,
    id: &str,
) -> Result<Node> {
    canonical_id(id)?;
    let descriptor = root_row(root, id)?;
    let document = hash_tree::get_manifest(http, host, token, &descriptor.hash, id)?;
    ensure!(
        document.rows.len() == descriptor.count as usize
            && document.total_size()? == descriptor.size,
        "Document manifest differs from its root entry"
    );
    let metadata_name = format!("{id}.metadata");
    let metadata_row = document
        .rows
        .iter()
        .find(|row| row.name == metadata_name)
        .ok_or_else(|| anyhow!("Document/folder has no metadata component"))?;
    ensure!(
        metadata_row.count == 0 && metadata_row.size <= JSON_LIMIT as u64,
        "Invalid metadata component"
    );
    let bytes = hash_tree::get_blob(
        http,
        host,
        token,
        &metadata_row.hash,
        &metadata_name,
        JSON_LIMIT,
    )?;
    ensure!(
        bytes.len() as u64 == metadata_row.size,
        "Metadata size mismatch"
    );
    let metadata = cloud::parse_json(&bytes)?;
    ensure!(metadata.is_object(), "Metadata must be a JSON object");
    let parent = metadata["parent"]
        .as_str()
        .ok_or_else(|| anyhow!("Metadata parent is missing"))?
        .to_owned();
    ensure!(
        !metadata["deleted"].as_bool().unwrap_or(false) && parent != "trash",
        "Moving trashed or deleted entries is not supported"
    );
    let folder = match metadata["type"].as_str() {
        Some("CollectionType") => true,
        Some("DocumentType") => false,
        _ => return Err(anyhow!("Entry has an unsupported metadata type")),
    };
    Ok(Node {
        document,
        metadata,
        parent,
        folder,
    })
}

fn checked_node<'a>(
    http: &impl Transport,
    host: &str,
    token: &str,
    root: &Manifest,
    cache: &'a mut HashMap<String, Node>,
    id: &str,
) -> Result<&'a Node> {
    if !cache.contains_key(id) {
        cache.insert(id.to_owned(), load_node(http, host, token, root, id)?);
    }
    Ok(cache.get(id).expect("node was inserted"))
}

fn validate_chain(
    http: &impl Transport,
    host: &str,
    token: &str,
    root: &Manifest,
    cache: &mut HashMap<String, Node>,
    start: &str,
    moved_id: &str,
) -> Result<()> {
    let mut current = start.to_owned();
    let mut seen = HashSet::new();
    while !current.is_empty() {
        ensure!(
            current != "trash",
            "Trash cannot be used in a move hierarchy"
        );
        ensure!(
            current != moved_id,
            "A folder cannot be moved into itself or its descendant"
        );
        ensure!(
            seen.insert(current.clone()),
            "Folder hierarchy contains a cycle"
        );
        let node = checked_node(http, host, token, root, cache, &current)?;
        ensure!(node.folder, "A parent in the hierarchy is not a folder");
        current = node.parent.clone();
    }
    Ok(())
}

/// Validate a prospective import destination against the exact root snapshot that will be
/// extended. This is shared by import upload so preparation and publication cannot disagree.
pub(crate) fn validate_destination(
    http: &impl Transport,
    host: &str,
    token: &str,
    root: &Manifest,
    parent: &str,
) -> Result<()> {
    ensure!(parent != "trash", "Importing into trash is not supported");
    if parent.is_empty() {
        return Ok(());
    }
    canonical_id(parent)?;
    validate_chain(http, host, token, root, &mut HashMap::new(), parent, "")
}

/// Move an active document or folder to an active folder. An empty parent means root.
///
/// All validation precedes immutable blob uploads. Root publication is generation guarded and
/// is attempted exactly once; callers must reconcile an uncertain error before trying again.
pub fn move_entry<T: Transport>(
    http: &T,
    host: &str,
    token: &str,
    id: &str,
    parent: &str,
) -> Result<()> {
    canonical_id(id)?;
    ensure!(
        parent != "trash",
        "Moving entries to trash is not supported"
    );
    if !parent.is_empty() {
        canonical_id(parent)?;
    }
    ensure!(id != parent, "An entry cannot be its own parent");

    let old_root = hash_tree::read_root(http, host, token)?;
    let mut root = hash_tree::get_manifest(http, host, token, &old_root.hash, ".")?;
    for row in &root.rows {
        canonical_id(&row.name).context("Root manifest contains an invalid entry identity")?;
    }
    let mut cache = HashMap::new();
    let source = checked_node(http, host, token, &root, &mut cache, id)?.clone();
    // Validate both the existing and requested ancestry. This rejects orphaned, non-folder,
    // deleted, trashed, and cyclic hierarchies before any cloud mutation.
    validate_chain(http, host, token, &root, &mut cache, &source.parent, id)?;
    validate_chain(http, host, token, &root, &mut cache, parent, id)?;
    if source.parent == parent {
        return Ok(());
    }

    let metadata_name = format!("{id}.metadata");
    let mut metadata = source.metadata;
    metadata["parent"] = Value::String(parent.to_owned());
    let metadata_bytes = serde_json::to_vec(&metadata)?;
    ensure!(
        metadata_bytes.len() <= JSON_LIMIT,
        "Updated metadata is too large"
    );
    let metadata_hash = job::hash(&metadata_bytes);

    let mut document = source.document;
    let metadata_row = document
        .rows
        .iter_mut()
        .find(|row| row.name == metadata_name)
        .expect("metadata row was validated");
    metadata_row.hash = metadata_hash.clone();
    metadata_row.size = metadata_bytes.len().try_into()?;
    let document_bytes = document.serialize()?;
    ensure!(
        document_bytes.len() <= JSON_LIMIT,
        "Updated document manifest is too large"
    );
    let document_hash = job::hash(&document_bytes);

    let updated_root_row = root
        .rows
        .iter_mut()
        .find(|row| row.name == id)
        .expect("root row was validated");
    updated_root_row.hash = document_hash.clone();
    updated_root_row.count = document.rows.len().try_into()?;
    updated_root_row.size = document.total_size()?;
    let root_bytes = root.serialize()?;
    ensure!(
        root_bytes.len() <= JSON_LIMIT,
        "Updated root manifest is too large"
    );
    let new_root_hash = job::hash(&root_bytes);

    hash_tree::put_blob(
        http,
        host,
        token,
        &metadata_hash,
        &metadata_name,
        metadata_bytes,
    )?;
    hash_tree::put_blob(
        http,
        host,
        token,
        &document_hash,
        &format!("{id}.docSchema"),
        document_bytes,
    )?;
    hash_tree::put_blob(
        http,
        host,
        token,
        &new_root_hash,
        "root.docSchema",
        root_bytes,
    )?;

    let body = serde_json::to_vec(&json!({
        "hash": new_root_hash,
        "generation": old_root.generation,
        "broadcast": true
    }))?;
    let response = http
        .request(
            host,
            "PUT",
            "/sync/v3/root",
            Some(token),
            Some(body),
            &[
                ("Content-Type", "application/json".into()),
                ("rm-filename", "roothash".into()),
                ("x-goog-hash", cloud::crc_header(new_root_hash.as_bytes())),
            ],
            JSON_LIMIT,
        )
        .context("Move publication failed or is uncertain. No automatic retry was made")?;
    let response = cloud::parse_json(&response)
        .context("Move publication response was invalid. No automatic retry was made")?;
    let generation = cloud::nonnegative(&response["generation"])
        .ok_or_else(|| anyhow!("Move publication response lacks a valid generation"))?;
    ensure!(
        generation > old_root.generation,
        "Move publication did not advance generation"
    );

    let verified = hash_tree::read_root(http, host, token)?;
    ensure!(
        verified.hash == new_root_hash && verified.generation == generation,
        "Published move does not match the fresh cloud root snapshot"
    );
    let verified_root = hash_tree::get_manifest(http, host, token, &verified.hash, ".")?;
    let verified_row = root_row(&verified_root, id)?;
    ensure!(
        verified_row.hash == document_hash,
        "Published entry does not contain the move"
    );
    let verified_document = hash_tree::get_manifest(http, host, token, &verified_row.hash, id)?;
    let verified_metadata = verified_document
        .rows
        .iter()
        .find(|row| row.name == metadata_name)
        .ok_or_else(|| anyhow!("Published entry lost its metadata component"))?;
    ensure!(
        verified_metadata.hash == metadata_hash,
        "Published metadata differs from the move"
    );
    Ok(())
}

#[cfg(test)]
#[path = "moves_tests.rs"]
mod tests;
