//! Schema-4 transport recovered in HASH-TREE-{SCHEMA4,BLOB,PUBLISH}.md.
use crate::{
    cloud::{self, Transport, JSON_LIMIT},
    job::{self, Job, State, TreeUpload},
    manifest::{Manifest, Row},
};
use anyhow::{anyhow, ensure, Context, Result};
use serde_json::json;
use std::path::Path;

#[cfg(test)]
#[path = "hash_tree_tests.rs"]
mod tests;

pub(crate) struct Root {
    pub hash: String,
    pub generation: u64,
}
pub(crate) fn read_root(http: &impl Transport, host: &str, token: &str) -> Result<Root> {
    let value = cloud::parse_json(&http.request(
        host,
        "GET",
        "/sync/v4/root",
        Some(token),
        None,
        &[("rm-filename", "roothash".into())],
        JSON_LIMIT,
    )?)?;
    ensure!(
        value["schemaVersion"].as_u64() == Some(4),
        "This account needs an unsupported hash-tree schema; no upload started"
    );
    ensure!(
        value.get("hasMissingHashes").is_none() || value["hasMissingHashes"] == json!(false),
        "Cloud reports missing hashes or an invalid recovery flag; refusing to extend the tree"
    );
    let generation = cloud::nonnegative(&value["generation"])
        .ok_or_else(|| anyhow!("Invalid root generation"))?;
    let hash = value["hash"]
        .as_str()
        .filter(|h| cloud::is_hash(h))
        .ok_or_else(|| anyhow!("Invalid root hash"))?
        .to_owned();
    Ok(Root { hash, generation })
}
fn unavailable(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<cloud::HttpError>()
        .is_some_and(|e| e.status == 404 && e.route == "/sync/v4/root")
}
pub(crate) fn get_blob(
    http: &impl Transport,
    host: &str,
    token: &str,
    hash: &str,
    filename: &str,
    limit: usize,
) -> Result<Vec<u8>> {
    ensure!(cloud::is_hash(hash), "Invalid blob address");
    let bytes = http.request(
        host,
        "GET",
        &format!("/sync/v3/files/{hash}"),
        Some(token),
        None,
        &[("rm-filename", filename.into())],
        limit,
    )?;
    ensure!(
        job::hash(&bytes) == hash,
        "Downloaded blob checksum differs from its address"
    );
    Ok(bytes)
}
pub(crate) fn get_manifest(
    http: &impl Transport,
    host: &str,
    token: &str,
    hash: &str,
    name: &str,
) -> Result<Manifest> {
    let filename = if name == "." {
        "root.docSchema".into()
    } else {
        format!("{name}.docSchema")
    };
    let bytes = get_blob(http, host, token, hash, &filename, JSON_LIMIT)?;
    let manifest = Manifest::parse(&bytes)?;
    ensure!(
        manifest.name == name,
        "Manifest identity differs from the expected node"
    );
    // Refuse any manifest that our writer would normalize in an unexpected way.
    ensure!(
        manifest.serialize()? == bytes,
        "Root/document manifest does not round-trip exactly"
    );
    Ok(manifest)
}
pub(crate) fn put_blob(
    http: &impl Transport,
    host: &str,
    token: &str,
    hash: &str,
    filename: &str,
    bytes: Vec<u8>,
) -> Result<()> {
    ensure!(
        job::hash(&bytes) == hash,
        "Upload bytes differ from their address"
    );
    let length = bytes.len();
    http.request(
        host,
        "PUT",
        &format!("/sync/v3/files/{hash}"),
        Some(token),
        Some(bytes),
        &[("rm-filename", filename.into())],
        JSON_LIMIT,
    )?;
    let verified = get_blob(http, host, token, hash, filename, length)?;
    ensure!(
        verified.len() == length,
        "Stored blob length differs from upload"
    );
    Ok(())
}

fn prepared_parent(job: &Job, payloads: &[Vec<u8>]) -> Result<String> {
    let metadata_name = format!("{}.metadata", job.uuid);
    let bytes = job
        .files
        .iter()
        .zip(payloads)
        .find(|(part, _)| part.path == metadata_name)
        .map(|(_, bytes)| bytes)
        .ok_or_else(|| anyhow!("Upload job has no metadata component"))?;
    let metadata = cloud::parse_json(bytes)?;
    metadata["parent"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("Upload metadata parent is missing"))
}
pub fn status(http: &impl Transport, host: &str, token: &str) -> Result<()> {
    let root = match read_root(http, host, token) {
        Ok(root) => root,
        Err(e) if unavailable(&e) => return cloud::status(http, host, token),
        Err(e) => return Err(e),
    };
    let manifest = get_manifest(http, host, token, &root.hash, ".")?;
    println!(
        "Authenticated schema-4 cloud access works. Root manifest verified with {} entries.",
        manifest.rows.len()
    );
    Ok(())
}
pub fn send(http: &impl Transport, host: &str, token: &str, dir: &Path) -> Result<()> {
    send_with_progress(http, host, token, dir, &mut |line| println!("{line}"))
}
/// [`send`], handing each progress line to `progress` instead of stdout, so a
/// caller can keep its own stdout machine-readable. The legacy gentree
/// fallback, used only when the schema-4 root route answers 404, still prints.
pub fn send_with_progress(
    http: &impl Transport,
    host: &str,
    token: &str,
    dir: &Path,
    progress: &mut dyn FnMut(&str),
) -> Result<()> {
    // Protocol selection is read-only. Only a root-route 404 permits gentree fallback.
    let root = match read_root(http, host, token) {
        Ok(root) => root,
        Err(e) if unavailable(&e) => {
            let job = Job::load(dir)?;
            let payloads = job.payloads(dir)?;
            let parent = prepared_parent(&job, &payloads)?;
            let epub = job
                .files
                .iter()
                .any(|part| part.path == format!("{}.epub", job.uuid));
            ensure!(
                parent.is_empty() && !epub,
                "This cloud account's legacy upload protocol supports only PDFs imported at root"
            );
            return crate::transfer::send(http, host, token, dir);
        }
        Err(e) => return Err(e).context("Upload has not started; root preflight failed"),
    };
    let _lock = job::lock(dir)?;
    let mut job = Job::load(dir)?;
    ensure!(
        job.state == State::Prepared,
        "This job was already attempted. Use verify; it will not be uploaded again"
    );
    let payloads = job.payloads(dir)?;
    let mut manifest = get_manifest(http, host, token, &root.hash, ".")?;
    let parent = prepared_parent(&job, &payloads)?;
    crate::moves::validate_destination(http, host, token, &manifest, &parent)
        .context("Import destination is not an active cloud folder")?;
    let id = job.uuid.to_string();
    ensure!(
        !manifest.rows.iter().any(|r| r.name == id),
        "Document identity already exists; refusing to replace it"
    );
    let document = Manifest::from_job(&job)?;
    let document_bytes = document.serialize()?;
    let document_hash = document.hash()?;
    manifest.rows.push(Row {
        hash: document_hash.clone(),
        flags: 0,
        name: id.clone(),
        count: document.rows.len().try_into()?,
        size: document.total_size()?,
    });
    let root_bytes = manifest.serialize()?;
    let new_root_hash = manifest.hash()?;
    ensure!(
        root_bytes.len() <= JSON_LIMIT,
        "New root manifest exceeds application size limit"
    );
    job.hash_tree = Some(TreeUpload {
        old_root_hash: root.hash,
        old_generation: root.generation,
        document_hash: document_hash.clone(),
        new_root_hash: new_root_hash.clone(),
    });
    job.state = State::Sending;
    job.save(dir)?;
    let result = (|| {
        progress(&format!(
            "Using schema-4 cloud transfer; preserving {} existing root entries.",
            manifest.rows.len() - 1
        ));
        for (index, (part, bytes)) in job.files.iter().zip(payloads).enumerate() {
            put_blob(http, host, token, &part.hash, &part.path, bytes)?;
            progress(&format!(
                "Uploaded and checked component {}/{}.",
                index + 1,
                job.files.len()
            ));
        }
        put_blob(
            http,
            host,
            token,
            &document_hash,
            &format!("{id}.docSchema"),
            document_bytes,
        )?;
        put_blob(
            http,
            host,
            token,
            &new_root_hash,
            "root.docSchema",
            root_bytes,
        )?;
        let body = serde_json::to_vec(
            &json!({"hash":new_root_hash,"generation":root.generation,"broadcast":true}),
        )?;
        let raw = http
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
            .context("Root publication failed or is uncertain. No automatic retry was made")?;
        let value = cloud::parse_json(&raw)?;
        job.completed_generation = Some(
            cloud::nonnegative(&value["generation"])
                .ok_or_else(|| anyhow!("Root publication response lacks a valid generation"))?,
        );
        job.state = State::Completed;
        job.save(dir)?;
        verify_locked_with(http, host, token, dir, &mut job, progress)
    })();
    result.with_context(|| {
        format!(
            "Upload journal retained at {}. Use verify to reconcile before another upload",
            dir.display()
        )
    })
}
pub fn verify(http: &impl Transport, host: &str, token: &str, dir: &Path) -> Result<()> {
    verify_with_progress(http, host, token, dir, &mut |line| println!("{line}"))
}
/// [`verify`], handing its progress line to `progress` instead of stdout.
pub fn verify_with_progress(
    http: &impl Transport,
    host: &str,
    token: &str,
    dir: &Path,
    progress: &mut dyn FnMut(&str),
) -> Result<()> {
    // Legacy jobs retain their original verification path.
    if Job::load(dir)?.hash_tree.is_none() {
        return crate::transfer::verify(http, host, token, dir);
    }
    let _lock = job::lock(dir)?;
    let mut job = Job::load(dir)?;
    ensure!(job.state != State::Prepared, "This job has not been sent");
    job.payloads(dir)?;
    verify_locked_with(http, host, token, dir, &mut job, progress)
}
pub(crate) fn verify_locked(
    http: &impl Transport,
    host: &str,
    token: &str,
    dir: &Path,
    job: &mut Job,
) -> Result<()> {
    verify_locked_with(http, host, token, dir, job, &mut |line| println!("{line}"))
}
fn verify_locked_with(
    http: &impl Transport,
    host: &str,
    token: &str,
    dir: &Path,
    job: &mut Job,
    progress: &mut dyn FnMut(&str),
) -> Result<()> {
    let tree = job
        .hash_tree
        .as_ref()
        .ok_or_else(|| anyhow!("Missing hash-tree journal"))?;
    let root = read_root(http, host, token)?;
    let manifest = get_manifest(http, host, token, &root.hash, ".")?;
    let id = job.uuid.to_string();
    let row = manifest.rows.iter().find(|r| r.name == id).ok_or_else(|| {
        anyhow!("Document is not in the current cloud root; publication is not verified")
    })?;
    ensure!(
        row.hash == tree.document_hash && row.flags == 0,
        "Cloud document changed; verification remains incomplete"
    );
    let expected = Manifest::from_job(job)?;
    ensure!(
        expected.hash()? == tree.document_hash
            && row.count as usize == job.files.len()
            && row.size == expected.total_size()?,
        "Cloud document descriptor differs from the prepared job"
    );
    let actual = get_manifest(http, host, token, &row.hash, &id)?;
    ensure!(
        actual.serialize()? == expected.serialize()?,
        "Cloud document manifest differs from the prepared job"
    );
    for part in &job.files {
        let bytes = get_blob(
            http,
            host,
            token,
            &part.hash,
            &part.path,
            part.size as usize,
        )?;
        ensure!(
            bytes.len() as u64 == part.size,
            "Cloud component length differs from the prepared job"
        );
    }
    job.state = State::Verified;
    job.verified_library_generation = Some(root.generation);
    job.save(dir)?;
    progress("Cloud copy verified byte for byte. Let your tablet sync and open the PDF to confirm device acceptance.");
    Ok(())
}
