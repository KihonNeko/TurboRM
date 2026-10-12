use crate::{
    cloud::{self, Session, Transport, JSON_LIMIT},
    job::{self, Job, State},
};
use anyhow::{anyhow, ensure, Context, Result};
use serde_json::Value;
use std::path::Path;

pub fn send(http: &impl Transport, host: &str, token: &str, dir: &Path) -> Result<()> {
    let _lock = job::lock(dir)?;
    let mut job = Job::load(dir)?;
    ensure!(job.state == State::Prepared,
        "This job was already attempted. Use verify to check the cloud; it will not be uploaded again");
    let payloads = job.payloads(dir)?;
    // Account access and response compatibility must succeed before a write.
    let listing = cloud::parse_json(&http.request(
        host,
        "GET",
        "/gentree/v1/GetEntries?generation=0",
        Some(token),
        None,
        &[],
        JSON_LIMIT,
    ).context("Upload has not started; cloud preflight failed. Run diagnose and keep this prepared job")?)?;
    cloud::library(&listing)?;
    ensure!(
        listing["entries"].is_null() || listing["entries"].is_array(),
        "Invalid entries response"
    );
    if let Some(entries) = listing["entries"].as_array() {
        ensure!(
            !entries.iter().any(|e| e["uuid"]
                .as_str()
                .and_then(|s| uuid::Uuid::parse_str(s).ok())
                == Some(job.uuid)),
            "Document identity already exists; refusing to replace it"
        );
    }
    // Persist before sending: a lost reply must never lead to a silent replay.
    job.state = State::Sending;
    job.save(dir)?;
    let result = send_once(http, host, token, dir, &mut job, payloads);
    result.with_context(|| format!("Upload journal retained at {}. Run verify on this directory before considering another upload", dir.display()))
}
fn send_once(
    http: &impl Transport,
    host: &str,
    token: &str,
    dir: &Path,
    job: &mut Job,
    payloads: Vec<Vec<u8>>,
) -> Result<()> {
    let headers = [
        ("Content-Type", "application/json".to_owned()),
        ("rm-path", job.uuid.to_string()),
    ];
    let raw = http.request(
        host,
        "POST",
        "/gentree/v1/EntrySession",
        Some(token),
        Some(serde_json::to_vec(&cloud::entry_request(job))?),
        &headers,
        JSON_LIMIT,
    )?;
    match cloud::session(&cloud::parse_json(&raw)?)? {
        Session::Completed(generation) => job.completed_generation = Some(generation),
        Session::Pending(id) => {
            for (index, (part, bytes)) in job.files.iter().zip(payloads).enumerate() {
                let path = cloud::query(
                    "/gentree/v1/PutFile",
                    &[("sessionId", &id), ("filePath", &part.path)],
                );
                let raw = http.request(
                    host,
                    "POST",
                    &path,
                    Some(token),
                    Some(bytes),
                    &[
                        ("rm-filename", part.path.clone()),
                        ("rm-path", part.path.clone()),
                    ],
                    JSON_LIMIT,
                )?;
                match cloud::session(&cloud::parse_json(&raw)?)? {
                    Session::Pending(returned) => {
                        ensure!(returned == id, "Upload session identity changed")
                    }
                    Session::Completed(generation) => job.completed_generation = Some(generation),
                }
                println!("Uploaded component {}/{}.", index + 1, job.files.len());
            }
            ensure!(
                job.completed_generation.is_some(),
                "All files were sent, but no explicit session completion was returned"
            );
        }
    }
    job.state = State::Completed;
    job.save(dir)?;
    verify_locked(http, host, token, dir, job)
}
pub fn verify(http: &impl Transport, host: &str, token: &str, dir: &Path) -> Result<()> {
    let _lock = job::lock(dir)?;
    let mut job = Job::load(dir)?;
    ensure!(job.state != State::Prepared, "This job has not been sent");
    job.payloads(dir)?;
    if job.hash_tree.is_some() {
        return crate::hash_tree::verify_locked(http, host, token, dir, &mut job);
    }
    verify_locked(http, host, token, dir, &mut job)
}
fn verify_locked(
    http: &impl Transport,
    host: &str,
    token: &str,
    dir: &Path,
    job: &mut Job,
) -> Result<()> {
    let id = job.uuid.to_string();
    let path = cloud::query(
        "/gentree/v1/GetFiles",
        &[("generation", "0"), ("entryUuid", &id)],
    );
    let value = cloud::parse_json(&http.request(
        host,
        "GET",
        &path,
        Some(token),
        None,
        &[("rm-path", id)],
        JSON_LIMIT,
    )?)?;
    let generation = validate_files(&value, job)?;
    // Verify the downloaded content, not just a server's advertised hashes.
    for part in &job.files {
        let path = cloud::query("/gentree/v1/GetFile", &[("fileHash", &part.hash)]);
        let bytes = http.request(
            host,
            "GET",
            &path,
            Some(token),
            None,
            &[("rm-path", part.path.clone())],
            part.size as usize,
        )?;
        ensure!(
            bytes.len() as u64 == part.size && job::hash(&bytes) == part.hash,
            "Downloaded bytes do not match the prepared document; verification remains incomplete"
        );
    }
    job.state = State::Verified;
    job.verified_library_generation = Some(generation);
    job.save(dir)?;
    println!(
        "Cloud copy verified byte for byte. Let your tablet sync to check that the PDF opens."
    );
    Ok(())
}
fn validate_files(value: &Value, job: &Job) -> Result<u64> {
    let generation = cloud::library(value)?;
    let entries = value["entries"]
        .as_array()
        .ok_or_else(|| anyhow!("Missing entry list"))?;
    ensure!(
        entries.len() == 1,
        "Expected exactly one document in verification response"
    );
    let entry = &entries[0];
    let id = entry["uuid"]
        .as_str()
        .and_then(|s| uuid::Uuid::parse_str(s).ok());
    ensure!(
        id == Some(job.uuid),
        "Verification returned a different document"
    );
    ensure!(entry.get("deletedAt").is_none(), "Document is deleted");
    ensure!(
        cloud::nonnegative(&entry["generation"]).is_some()
            && cloud::nonnegative(&entry["sizeBytes"]).is_some(),
        "Missing entry generation"
    );
    ensure!(
        entry["hash"].as_str() == Some(&job.entry_hash),
        "Cloud entry differs from the upload; it may be incomplete or changed by another client"
    );
    let files = entry["files"]
        .as_array()
        .ok_or_else(|| anyhow!("Missing full file list"))?;
    let active = files
        .iter()
        .filter(|f| f.get("deletedAt").is_none())
        .collect::<Vec<_>>();
    ensure!(
        active.len() == job.files.len(),
        "Cloud component set differs from this upload"
    );
    for expected in &job.files {
        let found = active
            .iter()
            .filter(|f| f["path"].as_str() == Some(&expected.path))
            .collect::<Vec<_>>();
        ensure!(found.len() == 1, "Missing or duplicate cloud component");
        let f = found[0];
        ensure!(
            f["hash"].as_str() == Some(&expected.hash)
                && cloud::nonnegative(&f["sizeBytes"]) == Some(expected.size)
                && cloud::nonnegative(&f["generation"]).is_some(),
            "Cloud component descriptor does not match"
        );
    }
    Ok(generation)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::{cell::RefCell, collections::VecDeque, fs};
    struct Mock {
        replies: RefCell<VecDeque<Result<Vec<u8>>>>,
        calls: RefCell<Vec<(String, String)>>,
    }
    impl Transport for Mock {
        fn request(
            &self,
            _host: &str,
            method: &str,
            path: &str,
            _token: Option<&str>,
            _body: Option<Vec<u8>>,
            _headers: &[(&str, String)],
            _limit: usize,
        ) -> Result<Vec<u8>> {
            self.calls.borrow_mut().push((method.into(), path.into()));
            self.replies
                .borrow_mut()
                .pop_front()
                .expect("Unexpected request")
        }
    }
    fn library() -> Value {
        json!({"generation":12,"hash":"ab".repeat(32)})
    }
    fn fixture() -> (tempfile::TempDir, Job) {
        let dir = tempfile::tempdir().unwrap();
        let uuid = uuid::Uuid::new_v4();
        let mut files = Vec::new();
        let mut digests = Vec::new();
        use sha2::{Digest, Sha256};
        for ext in ["pdf", "content", "metadata", "pagedata"] {
            let bytes = ext.as_bytes();
            let path = format!("{uuid}.{ext}");
            fs::write(dir.path().join(&path), bytes).unwrap();
            digests.push(Sha256::digest(bytes).into());
            files.push(job::Part {
                path,
                size: bytes.len() as u64,
                hash: job::hash(bytes),
            });
        }
        let job = Job {
            version: 1,
            uuid,
            entry_hash: job::aggregate(digests.into_iter()),
            files,
            state: State::Prepared,
            completed_generation: None,
            verified_library_generation: None,
            hash_tree: None,
        };
        job.save(dir.path()).unwrap();
        (dir, job)
    }
    fn file_response(job: &Job) -> Value {
        json!({"library":library(),"entries":[{"uuid":job.uuid,"generation":12,"hash":job.entry_hash,"sizeBytes":job.files.iter().map(|f|f.size).sum::<u64>(),
            "files":job.files.iter().map(|f|json!({"path":f.path,"hash":f.hash,"sizeBytes":f.size,"generation":12})).collect::<Vec<_>>()}]})
    }
    fn raw(v: Value) -> Result<Vec<u8>> {
        Ok(serde_json::to_vec(&v).unwrap())
    }
    #[test]
    fn successful_upload_requires_completion_and_download_verification() {
        let (dir, job) = fixture();
        let mut replies = VecDeque::from([
            raw(json!({"library":library(),"entries":[]})),
            raw(json!({"session":{"state":"pending","id":"opaque"}})),
        ]);
        for _ in 0..3 {
            replies.push_back(raw(json!({"session":{"state":"pending","id":"opaque"}})));
        }
        replies.push_back(raw(
            json!({"session":{"state":"completed","id":"opaque"},"library":library()}),
        ));
        replies.push_back(raw(file_response(&job)));
        for ext in ["pdf", "content", "metadata", "pagedata"] {
            replies.push_back(Ok(ext.as_bytes().to_vec()));
        }
        let http = Mock {
            replies: RefCell::new(replies),
            calls: RefCell::new(Vec::new()),
        };
        send(&http, "fake", "fake", dir.path()).unwrap();
        assert_eq!(Job::load(dir.path()).unwrap().state, State::Verified);
        assert!(http.replies.borrow().is_empty());
        assert!(http
            .calls
            .borrow()
            .iter()
            .any(|(_, p)| p.starts_with("/gentree/v1/GetFiles?generation=0&")));
        assert!(send(&http, "fake", "fake", dir.path()).is_err());
    }
    #[test]
    fn lost_session_response_blocks_replay_and_can_be_reconciled() {
        let (dir, job) = fixture();
        let http = Mock {
            replies: RefCell::new(VecDeque::from([
                raw(json!({"library":library(),"entries":[]})),
                Err(anyhow!("Lost response")),
            ])),
            calls: RefCell::new(vec![]),
        };
        assert!(send(&http, "fake", "fake", dir.path()).is_err());
        assert_eq!(Job::load(dir.path()).unwrap().state, State::Sending);
        let before = http.calls.borrow().len();
        assert!(send(&http, "fake", "fake", dir.path()).is_err());
        assert_eq!(before, http.calls.borrow().len());
        http.replies
            .borrow_mut()
            .push_back(raw(file_response(&job)));
        for ext in ["pdf", "content", "metadata", "pagedata"] {
            http.replies
                .borrow_mut()
                .push_back(Ok(ext.as_bytes().to_vec()));
        }
        verify(&http, "fake", "fake", dir.path()).unwrap();
        assert_eq!(Job::load(dir.path()).unwrap().state, State::Verified);
        assert!(http.calls.borrow()[before..]
            .iter()
            .all(|(method, _)| method == "GET"));
    }
    #[test]
    fn mismatched_remote_files_never_mark_verified() {
        let (_dir, job) = fixture();
        let mut v = file_response(&job);
        v["entries"][0]["files"][0]["sizeBytes"] = json!(0);
        assert!(validate_files(&v, &job).is_err());
        v = file_response(&job);
        v["entries"][0]["uuid"] = json!(uuid::Uuid::new_v4());
        assert!(validate_files(&v, &job).is_err());
        v = file_response(&job);
        v["entries"][0]["deletedAt"] = json!(123);
        assert!(validate_files(&v, &job).is_err());
    }
    #[test]
    fn corrupted_download_keeps_completed_but_unverified() {
        let (dir, mut job) = fixture();
        job.state = State::Completed;
        job.save(dir.path()).unwrap();
        let http = Mock {
            replies: RefCell::new(VecDeque::from([
                raw(file_response(&job)),
                Ok(b"bad".to_vec()),
            ])),
            calls: RefCell::new(vec![]),
        };
        assert!(verify(&http, "fake", "fake", dir.path()).is_err());
        assert_eq!(Job::load(dir.path()).unwrap().state, State::Completed);
    }
    #[test]
    fn all_http_successes_without_completion_remain_uncertain() {
        let (dir, _job) = fixture();
        let mut replies = VecDeque::from([raw(json!({"library":library(),"entries":[]}))]);
        for _ in 0..5 {
            replies.push_back(raw(json!({"session":{"state":"pending","id":"opaque"}})));
        }
        let http = Mock {
            replies: RefCell::new(replies),
            calls: RefCell::new(vec![]),
        };
        assert!(send(&http, "fake", "fake", dir.path()).is_err());
        assert_eq!(Job::load(dir.path()).unwrap().state, State::Sending);
        assert!(http.replies.borrow().is_empty());
        assert!(!http
            .calls
            .borrow()
            .iter()
            .any(|(_, p)| p.starts_with("/gentree/v1/GetFiles")));
    }
    #[test]
    fn failed_preflight_never_attempts_write_and_remains_prepared() {
        let (dir, _job) = fixture();
        let http = Mock {
            replies: RefCell::new(VecDeque::from([Err(anyhow!("HTTP 401"))])),
            calls: RefCell::new(vec![]),
        };
        assert!(send(&http, "fake", "fake", dir.path()).is_err());
        assert_eq!(Job::load(dir.path()).unwrap().state, State::Prepared);
        assert!(http
            .calls
            .borrow()
            .iter()
            .all(|(method, _)| method == "GET"));
    }
}
