use super::*;
use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::Value;
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    fs,
    path::PathBuf,
};

const HOST: &str = "eu.tectonic.remarkable.com";
const TOKEN: &str = "synthetic-test-token";
const OLD_GENERATION: u64 = 71;
const NEW_GENERATION: u64 = 79;
const EXISTING_ID: &str = "11111111-1111-4111-8111-111111111111";
const STALE_FOLDER_ID: &str = "22222222-2222-4222-8222-222222222222";

#[derive(Clone, Copy, PartialEq)]
enum Publication {
    Success,
    Conflict,
    LostBeforeCommit,
    LostAfterCommit,
}

struct Call {
    method: String,
    path: String,
    body: Option<Vec<u8>>,
}

struct Server {
    root: Value,
    blobs: HashMap<String, Vec<u8>>,
    written: HashSet<String>,
    calls: Vec<Call>,
    publication: Publication,
    corrupt_uploaded_read: bool,
}

struct Mock(RefCell<Server>);

impl Mock {
    fn new(publication: Publication) -> Self {
        let original = Manifest {
            name: ".".into(),
            flags: 0,
            rows: vec![Row {
                hash: "ab".repeat(32),
                flags: 0x8000_0012,
                name: EXISTING_ID.into(),
                count: 7,
                size: 987_654,
            }],
        };
        let hash = original.hash().unwrap();
        Self(RefCell::new(Server {
            root: json!({"hash":hash,"generation":OLD_GENERATION,"schemaVersion":4,"hasMissingHashes":false}),
            blobs: HashMap::from([(hash, original.serialize().unwrap())]),
            written: HashSet::new(),
            calls: Vec::new(),
            publication,
            corrupt_uploaded_read: false,
        }))
    }

    fn writes(&self) -> usize {
        self.0
            .borrow()
            .calls
            .iter()
            .filter(|c| c.method == "PUT")
            .count()
    }

    fn publications(&self) -> usize {
        self.0
            .borrow()
            .calls
            .iter()
            .filter(|c| c.method == "PUT" && c.path == "/sync/v3/root")
            .count()
    }

    fn manifest(&self) -> Manifest {
        let server = self.0.borrow();
        Manifest::parse(&server.blobs[server.root["hash"].as_str().unwrap()]).unwrap()
    }
}

impl Transport for Mock {
    fn request(
        &self,
        host: &str,
        method: &str,
        path: &str,
        token: Option<&str>,
        body: Option<Vec<u8>>,
        headers: &[(&str, String)],
        limit: usize,
    ) -> Result<Vec<u8>> {
        assert_eq!(host, HOST);
        assert_eq!(token, Some(TOKEN));
        let mut server = self.0.borrow_mut();
        server.calls.push(Call {
            method: method.into(),
            path: path.into(),
            body: body.clone(),
        });
        let header = |name: &str| {
            headers
                .iter()
                .find(|(key, _)| key.eq_ignore_ascii_case(name))
                .map(|(_, value)| value.as_str())
        };
        if path == "/sync/v4/root" && method == "GET" {
            assert_eq!(header("rm-filename"), Some("roothash"));
            assert!(body.is_none());
            return Ok(serde_json::to_vec(&server.root)?);
        }
        if let Some(hash) = path.strip_prefix("/sync/v3/files/") {
            assert!(cloud::is_hash(hash));
            assert!(header("rm-filename").is_some());
            match method {
                "PUT" => {
                    let bytes = body.expect("Blob write requires a body");
                    assert_eq!(job::hash(&bytes), hash);
                    server.blobs.insert(hash.into(), bytes);
                    server.written.insert(hash.into());
                    return Ok(Vec::new());
                }
                "GET" => {
                    assert!(body.is_none());
                    let mut bytes = server
                        .blobs
                        .get(hash)
                        .expect("Unseeded remote blob read")
                        .clone();
                    assert!(bytes.len() <= limit);
                    if server.corrupt_uploaded_read && server.written.contains(hash) {
                        bytes[0] ^= 1;
                    }
                    return Ok(bytes);
                }
                _ => panic!("Unexpected blob operation"),
            }
        }
        if path == "/sync/v3/root" && method == "PUT" {
            let bytes = body.expect("Root write requires a body");
            let value: Value = serde_json::from_slice(&bytes)?;
            let hash = value["hash"].as_str().expect("Root hash must be a string");
            assert_eq!(value["generation"], server.root["generation"]);
            assert_eq!(value["broadcast"], json!(true));
            assert_eq!(header("Content-Type"), Some("application/json"));
            assert_eq!(header("rm-filename"), Some("roothash"));
            let expected_crc = format!(
                "crc32c={}",
                STANDARD.encode(crc32c::crc32c(hash.as_bytes()).to_be_bytes())
            );
            assert_eq!(header("x-goog-hash"), Some(expected_crc.as_str()));
            assert_ne!(
                expected_crc,
                format!(
                    "crc32c={}",
                    STANDARD.encode(crc32c::crc32c(&bytes).to_be_bytes())
                )
            );
            assert!(
                server.blobs.contains_key(hash),
                "Root blob must exist before publication"
            );
            match server.publication {
                Publication::Conflict => {
                    return Err(cloud::HttpError {
                        status: 412,
                        route: path.into(),
                    }
                    .into())
                }
                Publication::LostBeforeCommit => {
                    return Err(anyhow!("Simulated disconnected request"))
                }
                Publication::Success | Publication::LostAfterCommit => {}
            }
            server.root["hash"] = json!(hash);
            server.root["generation"] = json!(NEW_GENERATION);
            if server.publication == Publication::LostAfterCommit {
                return Err(anyhow!("Simulated lost response after commit"));
            }
            return Ok(serde_json::to_vec(&json!({"generation":NEW_GENERATION}))?);
        }
        panic!("Unexpected request: {method} {path}");
    }
}

struct Fixture {
    _temp: tempfile::TempDir,
    dir: PathBuf,
    job: Job,
}

fn fixture() -> Fixture {
    fixture_with_parent("")
}

fn fixture_with_parent(parent: &str) -> Fixture {
    use lopdf::{dictionary, Object};
    let temp = tempfile::tempdir().unwrap();
    let mut pdf = lopdf::Document::with_version("1.5");
    let pages = pdf.new_object_id();
    let page = pdf.add_object(dictionary! {"Type"=>"Page", "Parent"=>pages,
    "MediaBox"=>vec![0.into(),0.into(),100.into(),100.into()]});
    pdf.objects.insert(
        pages,
        Object::Dictionary(dictionary! {"Type"=>"Pages", "Kids"=>vec![page.into()], "Count"=>1}),
    );
    let catalog = pdf.add_object(dictionary! {"Type"=>"Catalog", "Pages"=>pages});
    pdf.trailer.set("Root", catalog);
    let source = temp.path().join("synthetic.pdf");
    pdf.save(&source).unwrap();
    let dir = temp.path().join("job");
    let job = job::prepare_pdf(&source, &dir, Some("Synthetic transaction test"), parent).unwrap();
    Fixture {
        _temp: temp,
        dir,
        job,
    }
}

#[test]
fn stale_non_root_destination_is_rejected_before_any_write() {
    let fixture = fixture_with_parent(STALE_FOLDER_ID);
    let http = Mock::new(Publication::Success);
    let error = send(&http, HOST, TOKEN, &fixture.dir).unwrap_err();
    assert!(error.to_string().contains("destination"));
    assert_eq!(http.writes(), 0);
    assert_eq!(Job::load(&fixture.dir).unwrap().state, State::Prepared);
}

#[test]
fn publishes_after_six_verified_blobs_and_preserves_existing_records() {
    let fixture = fixture();
    let http = Mock::new(Publication::Success);
    send(&http, HOST, TOKEN, &fixture.dir).unwrap();
    let journal = Job::load(&fixture.dir).unwrap();
    assert_eq!(journal.state, State::Verified);
    assert_eq!(journal.completed_generation, Some(NEW_GENERATION));
    assert_eq!(journal.verified_library_generation, Some(NEW_GENERATION));
    assert_eq!(
        journal.hash_tree.as_ref().unwrap().old_generation,
        OLD_GENERATION
    );
    assert_eq!(http.publications(), 1);
    assert_eq!(http.writes(), 7);

    let root = http.manifest();
    assert_eq!(root.rows.len(), 2);
    let old = root.rows.iter().find(|r| r.name == EXISTING_ID).unwrap();
    assert_eq!(
        (&old.hash, old.flags, old.count, old.size),
        (&"ab".repeat(32), 0x8000_0012, 7, 987_654)
    );
    let row = root
        .rows
        .iter()
        .find(|r| r.name == fixture.job.uuid.to_string())
        .unwrap();
    assert_eq!(row.count, 4);
    let server = http.0.borrow();
    let document = Manifest::parse(&server.blobs[&row.hash]).unwrap();
    assert_eq!(document.rows.len(), 4);
    for part in &fixture.job.files {
        assert_eq!(
            server.blobs[&part.hash],
            fs::read(fixture.dir.join(&part.path)).unwrap()
        );
    }
    let publication = server
        .calls
        .iter()
        .position(|c| c.path == "/sync/v3/root")
        .unwrap();
    let writes = server.calls[..publication]
        .iter()
        .enumerate()
        .filter(|(_, c)| c.method == "PUT")
        .collect::<Vec<_>>();
    assert_eq!(writes.len(), 6);
    for (index, call) in writes {
        assert!(call.body.is_some());
        assert_eq!(server.calls[index + 1].method, "GET");
        assert_eq!(server.calls[index + 1].path, call.path);
    }
    assert!(
        !server
            .calls
            .iter()
            .any(|c| c.path.ends_with(&"ab".repeat(32))),
        "Existing document leaves should not need fetching or reuploading"
    );
}

#[test]
fn conflict_or_lost_request_is_not_retried_and_job_cannot_be_replayed() {
    for outcome in [Publication::Conflict, Publication::LostBeforeCommit] {
        let fixture = fixture();
        let http = Mock::new(outcome);
        assert!(send(&http, HOST, TOKEN, &fixture.dir).is_err());
        assert_eq!(Job::load(&fixture.dir).unwrap().state, State::Sending);
        assert_eq!(http.publications(), 1);
        let writes = http.writes();
        assert!(send(&http, HOST, TOKEN, &fixture.dir).is_err());
        assert_eq!(http.writes(), writes);
        assert!(verify(&http, HOST, TOKEN, &fixture.dir).is_err());
        assert_eq!(Job::load(&fixture.dir).unwrap().state, State::Sending);
    }
}

#[test]
fn lost_commit_response_is_reconciled_by_read_only_verification() {
    let fixture = fixture();
    let http = Mock::new(Publication::LostAfterCommit);
    assert!(send(&http, HOST, TOKEN, &fixture.dir).is_err());
    assert_eq!(Job::load(&fixture.dir).unwrap().state, State::Sending);
    assert_eq!(http.publications(), 1);
    let writes = http.writes();
    verify(&http, HOST, TOKEN, &fixture.dir).unwrap();
    let journal = Job::load(&fixture.dir).unwrap();
    assert_eq!(journal.state, State::Verified);
    assert_eq!(journal.completed_generation, None);
    assert_eq!(journal.verified_library_generation, Some(NEW_GENERATION));
    assert_eq!(http.writes(), writes);
    assert!(send(&http, HOST, TOKEN, &fixture.dir).is_err());
    assert_eq!(http.writes(), writes);
}

#[test]
fn corrupt_uploaded_blob_stops_before_root_publication() {
    let fixture = fixture();
    let http = Mock::new(Publication::Success);
    http.0.borrow_mut().corrupt_uploaded_read = true;
    assert!(send(&http, HOST, TOKEN, &fixture.dir).is_err());
    assert_eq!(http.writes(), 1);
    assert_eq!(http.publications(), 0);
    assert_eq!(Job::load(&fixture.dir).unwrap().state, State::Sending);
}

#[test]
fn invalid_root_preflight_keeps_prepared_job_and_performs_no_writes() {
    for invalid in [
        json!({"schemaVersion":4,"generation":71,"hash":"invalid"}),
        json!({"schemaVersion":4,"generation":-1,"hash":"ab".repeat(32)}),
        json!({"schemaVersion":3,"generation":71,"hash":"ab".repeat(32)}),
        json!({"schemaVersion":4,"generation":71,"hash":"ab".repeat(32),"hasMissingHashes":true}),
    ] {
        let fixture = fixture();
        let http = Mock::new(Publication::Success);
        http.0.borrow_mut().root = invalid;
        assert!(send(&http, HOST, TOKEN, &fixture.dir).is_err());
        assert_eq!(http.writes(), 0);
        let journal = Job::load(&fixture.dir).unwrap();
        assert_eq!(journal.state, State::Prepared);
        assert!(journal.hash_tree.is_none());
    }
}
