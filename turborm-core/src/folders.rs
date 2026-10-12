//! Additive folder creation in a schema-4 cloud library.
//!
//! A new folder is one new root entry with a single `.metadata` component,
//! exactly as current tablet firmware writes folders. Nothing else in the root is
//! changed: every existing row is carried over byte for byte, and the new root
//! is published with the generation read from the snapshot it extends.
use crate::{
    cloud::{self, HttpError, Transport, JSON_LIMIT},
    hash_tree, job,
    library::{Library, Listing, Snapshot},
    manifest::{Manifest, Row},
    moves,
};
use anyhow::{anyhow, ensure, Context, Result};
use serde_json::json;
use std::{
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;

/// Longest folder name accepted, in characters.
pub const MAX_NAME: usize = 255;
/// Most segments `ensure_folder_path` walks.
pub const MAX_DEPTH: usize = 32;
/// Publication attempts when the server reports a generation conflict (HTTP
/// 412). Each attempt starts again from a fresh root snapshot.
pub const MAX_ATTEMPTS: usize = 3;

/// A live folder with this name already exists under the requested parent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderExists {
    pub id: String,
    pub name: String,
}
impl std::fmt::Display for FolderExists {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "A folder named \"{}\" already exists here (ID {})",
            self.name, self.id
        )
    }
}
impl std::error::Error for FolderExists {}

/// One segment of an ensured folder path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderStep {
    pub id: String,
    pub name: String,
    /// True when this call created the folder; false when an existing live
    /// folder was reused.
    pub created: bool,
}

/// Trim and validate a folder name. Returns the name that will be stored.
pub fn validate_name(name: &str) -> Result<String> {
    let name = name.trim();
    ensure!(!name.is_empty(), "Enter a folder name");
    ensure!(
        name.chars().count() <= MAX_NAME,
        "A folder name can have at most {MAX_NAME} characters"
    );
    ensure!(
        !name.chars().any(char::is_control),
        "A folder name cannot contain control characters"
    );
    ensure!(!name.contains('/'), "A folder name cannot contain \"/\"");
    Ok(name.to_owned())
}

/// Split "A/B/C" into validated segment names. Leading and trailing slashes
/// are ignored; an empty segment in the middle ("A//B") is rejected.
pub fn split_path(path: &str) -> Result<Vec<String>> {
    let trimmed = path.trim().trim_matches('/');
    ensure!(!trimmed.is_empty(), "Enter a folder path such as \"A/B\"");
    let segments = trimmed
        .split('/')
        .map(|segment| {
            ensure!(
                !segment.trim().is_empty(),
                "The folder path \"{path}\" has an empty segment"
            );
            validate_name(segment)
        })
        .collect::<Result<Vec<_>>>()?;
    ensure!(
        segments.len() <= MAX_DEPTH,
        "A folder path can have at most {MAX_DEPTH} levels"
    );
    Ok(segments)
}

/// The key two folder names are compared by: trimmed, case-insensitive.
pub fn name_key(name: &str) -> String {
    name.trim().to_lowercase()
}

/// The live folder named `name` directly under `parent` in a listing, if any.
/// With several, the lowest ID wins so repeated calls agree.
pub fn find_child_folder<'a>(
    entries: &'a [crate::library::Entry],
    parent: &str,
    name: &str,
) -> Option<&'a crate::library::Entry> {
    let wanted = name_key(name);
    entries
        .iter()
        .filter(|entry| {
            entry.kind == "folder"
                && !entry.deleted
                && entry.parent == parent
                && entry.hierarchy_warning.is_none()
                && name_key(&entry.title) == wanted
        })
        .min_by(|a, b| a.id.cmp(&b.id))
}

/// The exact `.metadata` bytes current firmware writes for a folder: Qt's
/// indented JSON (four spaces, keys in sorted order) and a final newline.
pub fn folder_metadata(name: &str, parent: &str, created: &str, modified: &str) -> Vec<u8> {
    let text = |value: &str| json!(value).to_string();
    format!(
        "{{\n    \"createdTime\": {},\n    \"lastModified\": {},\n    \"new\": false,\n    \"parent\": {},\n    \"pinned\": false,\n    \"source\": \"\",\n    \"type\": \"CollectionType\",\n    \"visibleName\": {}\n}}\n",
        text(created),
        text(modified),
        text(parent),
        text(name)
    )
    .into_bytes()
}

fn now_millis() -> Result<String> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_millis()
        .to_string())
}

fn conflict(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<HttpError>()
        .is_some_and(|e| e.status == 412 && e.route == "/sync/v3/root")
}

/// Reads the current root and the listing that goes with it.
fn snapshot_listing<T: Transport + Sync>(
    http: &T,
    host: &str,
    token: &str,
    cache: Option<&PathBuf>,
) -> Result<(hash_tree::Root, Manifest, Listing)> {
    let root = hash_tree::read_root(http, host, token)?;
    let manifest = hash_tree::get_manifest(http, host, token, &root.hash, ".")?;
    for row in &manifest.rows {
        ensure!(
            Uuid::parse_str(&row.name).is_ok_and(|id| !id.is_nil() && id.to_string() == row.name),
            "Root manifest contains an invalid entry identity"
        );
    }
    let library = Library::new(http, host, token, cache.cloned());
    let listing = library.list(
        &Snapshot {
            generation: root.generation,
            manifest: manifest.clone(),
        },
        4,
    )?;
    Ok((root, manifest, listing))
}

/// Everything needed to publish one new folder and to check it afterwards.
struct Proposal {
    id: String,
    metadata: Vec<u8>,
    metadata_hash: String,
    document: Vec<u8>,
    document_hash: String,
    root: Vec<u8>,
    root_hash: String,
}

fn propose(root: &Manifest, id: &str, metadata: Vec<u8>) -> Result<Proposal> {
    ensure!(
        !root.rows.iter().any(|row| row.name == id),
        "The new folder identity already exists; refusing to replace it"
    );
    ensure!(metadata.len() <= JSON_LIMIT, "Folder metadata is too large");
    let metadata_hash = job::hash(&metadata);
    let document = Manifest {
        name: id.to_owned(),
        flags: 0,
        rows: vec![Row {
            hash: metadata_hash.clone(),
            flags: 0,
            name: format!("{id}.metadata"),
            count: 0,
            size: metadata.len().try_into()?,
        }],
    };
    let document_bytes = document.serialize()?;
    let document_hash = job::hash(&document_bytes);
    let mut extended = root.clone();
    extended.rows.push(Row {
        hash: document_hash.clone(),
        flags: 0,
        name: id.to_owned(),
        count: 1,
        size: document.total_size()?,
    });
    // Only an addition: every existing row is carried over unchanged.
    ensure!(
        extended.rows.len() == root.rows.len() + 1
            && extended.flags == root.flags
            && root.rows.iter().zip(&extended.rows).all(|(a, b)| a == b),
        "The proposed root would change an existing entry"
    );
    let root_bytes = extended.serialize()?;
    ensure!(
        root_bytes.len() <= JSON_LIMIT,
        "New root manifest exceeds the application size limit"
    );
    Ok(Proposal {
        id: id.to_owned(),
        metadata_hash,
        metadata,
        document_hash,
        document: document_bytes,
        root_hash: job::hash(&root_bytes),
        root: root_bytes,
    })
}

enum Published {
    Done,
    Conflict,
}

fn publish<T: Transport>(
    http: &T,
    host: &str,
    token: &str,
    generation: u64,
    proposal: &Proposal,
) -> Result<Published> {
    let id = &proposal.id;
    hash_tree::put_blob(
        http,
        host,
        token,
        &proposal.metadata_hash,
        &format!("{id}.metadata"),
        proposal.metadata.clone(),
    )?;
    hash_tree::put_blob(
        http,
        host,
        token,
        &proposal.document_hash,
        &format!("{id}.docSchema"),
        proposal.document.clone(),
    )?;
    hash_tree::put_blob(
        http,
        host,
        token,
        &proposal.root_hash,
        "root.docSchema",
        proposal.root.clone(),
    )?;
    let body = serde_json::to_vec(&json!({
        "hash": proposal.root_hash,
        "generation": generation,
        "broadcast": true
    }))?;
    let response = match http.request(
        host,
        "PUT",
        "/sync/v3/root",
        Some(token),
        Some(body),
        &[
            ("Content-Type", "application/json".into()),
            ("rm-filename", "roothash".into()),
            (
                "x-goog-hash",
                cloud::crc_header(proposal.root_hash.as_bytes()),
            ),
        ],
        JSON_LIMIT,
    ) {
        Ok(response) => response,
        // A clear precondition failure: the root moved on and ours was not
        // adopted. The caller may start again from a fresh snapshot.
        Err(error) if conflict(&error) => return Ok(Published::Conflict),
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "Folder publication failed or is uncertain. No automatic retry was made; \
                     check with `list` whether folder {id} exists before trying again"
                )
            })
        }
    };
    let response = cloud::parse_json(&response)
        .context("Folder publication response was invalid. No automatic retry was made")?;
    let published = cloud::nonnegative(&response["generation"])
        .ok_or_else(|| anyhow!("Folder publication response lacks a valid generation"))?;
    ensure!(
        published > generation,
        "Folder publication did not advance the generation"
    );
    Ok(Published::Done)
}

/// Re-read the cloud and confirm the new folder is there with exactly the
/// bytes that were proposed.
fn verify<T: Transport>(http: &T, host: &str, token: &str, proposal: &Proposal) -> Result<()> {
    let id = &proposal.id;
    let current = hash_tree::read_root(http, host, token)?;
    let root = hash_tree::get_manifest(http, host, token, &current.hash, ".")?;
    let row = root
        .rows
        .iter()
        .find(|row| row.name == *id)
        .ok_or_else(|| anyhow!("The new folder {id} is not in the fresh cloud root"))?;
    ensure!(
        row.hash == proposal.document_hash && row.count == 1,
        "The published folder entry differs from the proposal"
    );
    let document = hash_tree::get_manifest(http, host, token, &row.hash, id)?;
    ensure!(
        document.serialize()? == proposal.document,
        "The published folder manifest differs from the proposal"
    );
    let metadata = hash_tree::get_blob(
        http,
        host,
        token,
        &proposal.metadata_hash,
        &format!("{id}.metadata"),
        JSON_LIMIT,
    )?;
    ensure!(
        metadata == proposal.metadata,
        "The published folder metadata differs from the proposal"
    );
    Ok(())
}

fn create_once<T: Transport + Sync>(
    http: &T,
    host: &str,
    token: &str,
    cache: Option<&PathBuf>,
    parent: &str,
    name: &str,
    id: &str,
) -> Result<Option<String>> {
    let (root, manifest, listing) = snapshot_listing(http, host, token, cache)?;
    moves::validate_destination(http, host, token, &manifest, parent)
        .context("The parent is not an active folder")?;
    if let Some(existing) = find_child_folder(&listing.entries, parent, name) {
        return Err(FolderExists {
            id: existing.id.clone(),
            name: existing.title.clone(),
        }
        .into());
    }
    let now = now_millis()?;
    let proposal = propose(&manifest, id, folder_metadata(name, parent, &now, &now))?;
    match publish(http, host, token, root.generation, &proposal)? {
        Published::Conflict => Ok(None),
        Published::Done => {
            verify(http, host, token, &proposal)?;
            Ok(Some(proposal.id))
        }
    }
}

/// Create one new folder named `name` under `parent` ("" for the library
/// root) and return its ID.
///
/// Only adds: no existing entry is read for modification or rewritten. The
/// parent must be the root or an active folder outside the trash. A live
/// folder with the same name (ignoring case) under the same parent is refused
/// with [`FolderExists`]; use [`ensure_folder_path`] to reuse it instead. A
/// generation conflict (HTTP 412) starts again from a fresh snapshot, at most
/// [`MAX_ATTEMPTS`] times; any other publication error is reported without a
/// retry.
pub fn create_folder<T: Transport + Sync>(
    http: &T,
    host: &str,
    token: &str,
    cache: Option<PathBuf>,
    parent: &str,
    name: &str,
) -> Result<String> {
    let name = validate_name(name)?;
    ensure!(parent != "trash", "Folders cannot be created in the trash");
    if !parent.is_empty() {
        ensure!(
            Uuid::parse_str(parent).is_ok_and(|id| !id.is_nil() && id.to_string() == parent),
            "The parent must be empty (library root) or a canonical folder UUID"
        );
    }
    // One identity for every attempt: a 412 means the earlier proposal was
    // not adopted, so the same ID is still free.
    let id = Uuid::new_v4().to_string();
    for _ in 0..MAX_ATTEMPTS {
        if let Some(id) = create_once(http, host, token, cache.as_ref(), parent, &name, &id)? {
            return Ok(id);
        }
    }
    Err(anyhow!(
        "The cloud library kept changing during {MAX_ATTEMPTS} attempts; no folder was created. Try again"
    ))
}

/// Walk `path` ("A/B/C") from `parent`, reusing each live folder that already
/// exists (ignoring case) and creating the missing ones. Returns one step per
/// segment; the last step is the requested folder.
pub fn ensure_folder_path<T: Transport + Sync>(
    http: &T,
    host: &str,
    token: &str,
    cache: Option<PathBuf>,
    parent: &str,
    path: &str,
) -> Result<Vec<FolderStep>> {
    let segments = split_path(path)?;
    let (_, manifest, listing) = snapshot_listing(http, host, token, cache.as_ref())?;
    moves::validate_destination(http, host, token, &manifest, parent)
        .context("The parent is not an active folder")?;
    let mut steps = Vec::with_capacity(segments.len());
    let mut current = parent.to_owned();
    let mut creating = false;
    for segment in segments {
        let existing = (!creating)
            .then(|| find_child_folder(&listing.entries, &current, &segment))
            .flatten();
        let step = match existing {
            Some(entry) => FolderStep {
                id: entry.id.clone(),
                name: entry.title.clone(),
                created: false,
            },
            None => match create_folder(http, host, token, cache.clone(), &current, &segment) {
                Ok(id) => {
                    creating = true;
                    FolderStep {
                        id,
                        name: segment,
                        created: true,
                    }
                }
                // Another client created it since the listing: reuse it.
                Err(error) => match error.downcast_ref::<FolderExists>() {
                    Some(exists) => FolderStep {
                        id: exists.id.clone(),
                        name: exists.name.clone(),
                        created: false,
                    },
                    None => return Err(error),
                },
            },
        };
        current = step.id.clone();
        steps.push(step);
    }
    Ok(steps)
}

#[cfg(test)]
#[path = "folders_tests.rs"]
mod tests;
