use anyhow::{anyhow, bail, ensure, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use reqwest::{blocking::Client, redirect::Policy, Url};
use serde_json::{json, Value};
use std::{io::Read, time::Duration};

pub const BASE: &str = "internal.cloud.remarkable.com";
pub const JSON_LIMIT: usize = 16 * 1024 * 1024;

#[derive(Debug)]
pub struct HttpError {
    pub status: u16,
    pub route: String,
}
impl std::fmt::Display for HttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Cloud returned HTTP {} for {} (response body omitted)",
            self.status, self.route
        )
    }
}
impl std::error::Error for HttpError {}

pub trait Transport {
    #[allow(clippy::too_many_arguments)]
    fn request(
        &self,
        host: &str,
        method: &str,
        path: &str,
        token: Option<&str>,
        body: Option<Vec<u8>>,
        headers: &[(&str, String)],
        limit: usize,
    ) -> Result<Vec<u8>>;
}

/// Capability boundary for library browsing and downloads: reject every mutation
/// before it can reach the underlying authenticated transport.
pub struct ReadOnly<'a, T: Transport>(&'a T);
impl<'a, T: Transport> ReadOnly<'a, T> {
    pub fn new(transport: &'a T) -> Self {
        Self(transport)
    }
}
impl<T: Transport> Transport for ReadOnly<'_, T> {
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
        ensure!(
            method == "GET" && body.is_none(),
            "Library transport is read-only; mutation refused before network access"
        );
        self.0
            .request(host, method, path, token, body, headers, limit)
    }
}

pub struct Http(Client, String);
impl Http {
    pub fn new() -> Result<Self> {
        let client = Client::builder()
            .redirect(Policy::none())
            .retry(reqwest::retry::never())
            .connect_timeout(Duration::from_secs(20))
            .timeout(Duration::from_secs(180))
            .user_agent(concat!("turborm/", env!("CARGO_PKG_VERSION")))
            .https_only(true)
            .build()
            .map_err(|_| anyhow!("Could not initialize HTTPS transport"))?;
        Ok(Self(client, uuid::Uuid::new_v4().to_string()))
    }
    /// Read-only route diagnostics. Never prints tokens, document IDs or response bodies.
    pub fn diagnose(&self, host: &str, token: &str) -> Result<()> {
        crate::auth::vendor_host(host)?;
        println!("Cloud host: {host}");
        for path in ["/gentree/v1/GetEntries?generation=0", "/sync/v4/root"] {
            let mut request = self
                .0
                .get(format!("https://{host}{path}"))
                .bearer_auth(token)
                .header("Accept-Encoding", "identity")
                .header("rm-sync-id", &self.1)
                .header("rm-batch-number", "1");
            if path == "/sync/v4/root" {
                request = request.header("rm-filename", "roothash");
            }
            let response = request
                .send()
                .map_err(|_| anyhow!("Diagnostic HTTPS request failed (details omitted)"))?;
            let status = response.status().as_u16();
            let mut bytes = Vec::new();
            response
                .take(JSON_LIMIT as u64 + 1)
                .read_to_end(&mut bytes)
                .map_err(|_| anyhow!("Diagnostic response read failed"))?;
            ensure!(
                bytes.len() <= JSON_LIMIT,
                "Diagnostic response exceeds size limit"
            );
            let report = diagnostic_report(path, status, &bytes);
            println!("{report}");
        }
        Ok(())
    }
    #[allow(clippy::too_many_arguments)]
    pub fn request(
        &self,
        host: &str,
        method: &str,
        path: &str,
        token: Option<&str>,
        body: Option<Vec<u8>>,
        headers: &[(&str, String)],
        limit: usize,
    ) -> Result<Vec<u8>> {
        <Self as Transport>::request(self, host, method, path, token, body, headers, limit)
    }
}
impl Transport for Http {
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
        crate::auth::vendor_host(host)?;
        let route = path.split('?').next().unwrap_or("");
        ensure!(
            matches!(
                (method, route),
                (
                    "GET",
                    "/discovery/v1/endpoints"
                        | "/sync/v4/root"
                        | "/gentree/v1/GetEntries"
                        | "/gentree/v1/GetFiles"
                        | "/gentree/v1/GetFile"
                ) | (
                    "POST",
                    "/token/json/2/device/new"
                        | "/token/json/2/user/new"
                        | "/gentree/v1/EntrySession"
                        | "/gentree/v1/PutFile"
                )
            ) || (matches!(method, "GET" | "PUT")
                && route.strip_prefix("/sync/v3/files/").is_some_and(is_hash))
                || (method == "PUT" && route == "/sync/v3/root"),
            "Unsupported operation"
        );
        let url = Url::parse(&format!("https://{host}{path}"))?;
        ensure!(url.host_str() == Some(host), "Request destination changed");
        let method = reqwest::Method::from_bytes(method.as_bytes())?;
        let mut request = self
            .0
            .request(method, url)
            .header("Accept-Encoding", "identity");
        if route == "/token/json/2/device/new" {
            ensure!(
                token.is_none(),
                "Registration must not carry an existing token"
            );
            request = request.header("Authorization", "Bearer");
        } else if route == "/discovery/v1/endpoints" {
            ensure!(token.is_none() && host == BASE, "Invalid discovery target");
        } else {
            let token = token.ok_or_else(|| anyhow!("Missing authentication"))?;
            ensure!(
                !token.is_empty()
                    && token.len() <= 32768
                    && token.bytes().all(|b| (33..=126).contains(&b)),
                "Invalid authentication representation"
            );
            request = request.bearer_auth(token);
        }
        if route.starts_with("/gentree/v1/") || route.starts_with("/sync/") {
            request = request
                .header("rm-sync-id", &self.1)
                .header("rm-batch-number", "1");
        }
        request = payload_request(request, route, body, headers);
        let response = request.send().map_err(|_| anyhow!(
            "HTTPS request failed; a write may have reached the server. No automatic retry was made"))?;
        let status = response.status();
        if !status.is_success() {
            return Err(HttpError {
                status: status.as_u16(),
                route: if route.starts_with("/sync/v3/files/") {
                    "/sync/v3/files/{hash}"
                } else {
                    route
                }
                .into(),
            }
            .into());
        }
        let mut raw = Vec::new();
        response
            .take(limit as u64 + 1)
            .read_to_end(&mut raw)
            .map_err(|_| anyhow!("Response read failed; write outcome may be unknown"))?;
        ensure!(
            raw.len() <= limit,
            "Response exceeds the application size limit"
        );
        Ok(raw)
    }
}

fn diagnostic_report(path: &str, status: u16, bytes: &[u8]) -> Value {
    let mut report = json!({"path":path,"status":status,"response_bytes":bytes.len()});
    if let Ok(value) = serde_json::from_slice::<Value>(bytes) {
        report["json_object"] = json!(value.is_object());
        for key in ["hash", "generation", "schemaVersion", "library", "entries"] {
            report[format!("has_{key}")] = json!(value.get(key).is_some());
        }
        report["root_generation_valid"] = json!(nonnegative(&value["generation"]).is_some());
        report["root_hash_valid"] = json!(value["hash"].as_str().is_some_and(is_hash));
        report["root_hash_empty"] = json!(value["hash"].as_str() == Some(""));
        report["gentree_library_valid"] = json!(library(&value).is_ok());
        if let Some(v) = value.get("schemaVersion").and_then(Value::as_u64) {
            report["schemaVersion"] = json!(v);
        }
        if let Some(v) = value.get("hasMissingHashes").and_then(Value::as_bool) {
            report["hasMissingHashes"] = json!(v);
        }
    }
    report
}

fn payload_request(
    mut request: reqwest::blocking::RequestBuilder,
    route: &str,
    body: Option<Vec<u8>>,
    headers: &[(&str, String)],
) -> reqwest::blocking::RequestBuilder {
    if let Some(body) = body {
        let content_type = headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("content-type"))
            .map(|(_, value)| value.as_str())
            .unwrap_or("application/octet-stream");
        request = request.header("Content-Type", content_type);
        if route == "/gentree/v1/PutFile"
            || route.starts_with("/token/json/2/")
            || route.starts_with("/sync/v3/files/")
        {
            request = request.header("x-goog-hash", crc_header(&body));
        }
        request = request.body(body);
    }
    for (name, value) in headers {
        if !name.eq_ignore_ascii_case("content-type") {
            request = request.header(*name, value);
        }
    }
    request
}
pub fn crc_header(bytes: &[u8]) -> String {
    format!(
        "crc32c={}",
        STANDARD.encode(crc32c::crc32c(bytes).to_be_bytes())
    )
}
pub fn query(route: &str, pairs: &[(&str, &str)]) -> String {
    let mut url = Url::parse("https://internal.cloud.remarkable.com").unwrap();
    url.set_path(route);
    url.query_pairs_mut().extend_pairs(pairs.iter().copied());
    format!("{}?{}", url.path(), url.query().unwrap_or(""))
}
pub fn is_hash(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
pub fn parse_json(raw: &[u8]) -> Result<Value> {
    serde_json::from_slice(raw).map_err(|_| anyhow!("Cloud returned invalid JSON (body omitted)"))
}
pub fn nonnegative(value: &Value) -> Option<u64> {
    value.as_i64().filter(|n| *n >= 0).map(|n| n as u64)
}
pub fn library(value: &Value) -> Result<u64> {
    let generation = nonnegative(&value["library"]["generation"])
        .ok_or_else(|| anyhow!("Missing or invalid library generation"))?;
    ensure!(
        value["library"]["hash"].as_str().is_some_and(is_hash),
        "Missing or invalid library hash"
    );
    Ok(generation)
}
#[derive(Debug, PartialEq)]
pub enum Session {
    Pending(String),
    Completed(u64),
}
pub fn session(value: &Value) -> Result<Session> {
    let id = value["session"]["id"]
        .as_str()
        .filter(|v| !v.is_empty() && v.len() <= 4096)
        .ok_or_else(|| anyhow!("Missing or invalid session identity"))?;
    match value["session"]["state"].as_str() {
        Some("pending") => Ok(Session::Pending(id.to_owned())),
        Some("completed") => Ok(Session::Completed(library(value)?)),
        _ => bail!("Unrecognized session state"),
    }
}
pub fn status(http: &impl Transport, host: &str, token: &str) -> Result<()> {
    let value = parse_json(&http.request(
        host,
        "GET",
        "/gentree/v1/GetEntries?generation=0",
        Some(token),
        None,
        &[],
        JSON_LIMIT,
    )?)?;
    let generation = library(&value)?;
    ensure!(
        value["entries"].is_null() || value["entries"].is_array(),
        "Unexpected entries representation"
    );
    let count = value["entries"].as_array().map_or(0, Vec::len);
    println!("Authenticated cloud access works. Response contains {count} entries at generation {generation}.");
    Ok(())
}
pub fn inspect_tree(http: &impl Transport, host: &str, token: &str) -> Result<()> {
    let root = parse_json(&http.request(
        host,
        "GET",
        "/sync/v4/root",
        Some(token),
        None,
        &[("rm-filename", "roothash".into())],
        JSON_LIMIT,
    )?)?;
    ensure!(
        nonnegative(&root["generation"]).is_some(),
        "Invalid root generation"
    );
    let hash = root["hash"]
        .as_str()
        .filter(|h| is_hash(h))
        .ok_or_else(|| anyhow!("Invalid root hash"))?;
    let bytes = http.request(
        host,
        "GET",
        &format!("/sync/v3/files/{hash}"),
        Some(token),
        None,
        &[("rm-filename", "root.docSchema".into())],
        JSON_LIMIT,
    )?;
    let text = std::str::from_utf8(&bytes).map_err(|_| anyhow!("Root manifest is not UTF-8"))?;
    let version = text.lines().next().and_then(|s| s.parse::<u32>().ok());
    let fields: Vec<usize> = text.lines().skip(1).map(|s| s.split(':').count()).collect();
    println!(
        "{}",
        json!({"root_manifest_version":version,"manifest_bytes":bytes.len(),
        "sha256_matches_address":crate::job::hash(&bytes)==hash,"lines_after_version":fields.len(),
        "field_counts":fields.iter().fold(std::collections::BTreeMap::new(), |mut m,n| { *m.entry(*n).or_insert(0usize)+=1;m }),
        "final_newline":text.ends_with('\n')})
    );
    Ok(())
}
pub fn entry_request(job: &crate::job::Job) -> Value {
    json!({"uuid": job.uuid, "preEntryGeneration": 0, "postEntryHash": job.entry_hash,
        "ops": job.files.iter().map(|f| json!({"hash": f.hash, "path": f.path,
            "sizeBytes": f.size, "op": 0})).collect::<Vec<_>>()})
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn read_only_transport_blocks_every_write_before_network_access() {
        struct Never;
        impl Transport for Never {
            fn request(
                &self,
                _host: &str,
                _method: &str,
                _path: &str,
                _token: Option<&str>,
                _body: Option<Vec<u8>>,
                _headers: &[(&str, String)],
                _limit: usize,
            ) -> Result<Vec<u8>> {
                panic!("Mutation must not reach the network transport");
            }
        }
        let transport = ReadOnly::new(&Never);
        for method in ["PUT", "POST", "PATCH", "DELETE"] {
            assert!(transport
                .request(BASE, method, "/sync/v3/root", None, None, &[], 100)
                .is_err());
        }
        assert!(transport
            .request(BASE, "GET", "/sync/v4/root", None, Some(vec![]), &[], 100)
            .is_err());
    }
    #[test]
    fn diagnostics_keep_private_values_out_of_output() {
        let input = json!({"generation":123,"hash":"ab".repeat(32),"schemaVersion":3,
            "email":"private@example.test","token":"PRIVATE_TOKEN_VALUE","entries":[{"uuid":"PRIVATE_ID"}]});
        let report = diagnostic_report("/sync/v4/root", 200, &serde_json::to_vec(&input).unwrap());
        assert_eq!(report["root_generation_valid"], json!(true));
        assert_eq!(report["root_hash_valid"], json!(true));
        assert_eq!(report["schemaVersion"], json!(3));
        let text = report.to_string();
        for secret in [
            "private@example.test",
            "PRIVATE_TOKEN_VALUE",
            "PRIVATE_ID",
            &"ab".repeat(32),
        ] {
            assert!(!text.contains(secret));
        }
        assert!(diagnostic_report("/sync/v4/root", 404, b"private text")
            .get("root_hash_valid")
            .is_none());
    }
    #[test]
    fn request_has_one_content_type_and_route_specific_checksum() {
        let client = Client::new();
        let session = payload_request(
            client.post("https://internal.cloud.remarkable.com/gentree/v1/EntrySession"),
            "/gentree/v1/EntrySession",
            Some(b"{}".to_vec()),
            &[("Content-Type", "application/json".into())],
        )
        .build()
        .unwrap();
        assert_eq!(session.headers().get_all("content-type").iter().count(), 1);
        assert_eq!(session.headers()["content-type"], "application/json");
        assert!(session.headers().get("x-goog-hash").is_none());
        for route in ["/gentree/v1/PutFile", "/token/json/2/device/new"] {
            let request = payload_request(
                client.post(format!("https://internal.cloud.remarkable.com{route}")),
                route,
                Some(b"123456789".to_vec()),
                &[],
            )
            .build()
            .unwrap();
            assert_eq!(request.headers().get_all("content-type").iter().count(), 1);
            assert_eq!(
                request.headers()["content-type"],
                "application/octet-stream"
            );
            assert_eq!(request.headers()["x-goog-hash"], "crc32c=4waSgw==");
        }
    }
    #[test]
    fn checksum_vectors() {
        assert_eq!(crc_header(b"123456789"), "crc32c=4waSgw==");
        assert_eq!(crc_header(b""), "crc32c=AAAAAA==");
    }
    #[test]
    fn root_publication_uses_hash_text_checksum_not_json_checksum() {
        let hash = "ab".repeat(32);
        let body =
            serde_json::to_vec(&json!({"hash":hash,"generation":123,"broadcast":true})).unwrap();
        let request = payload_request(
            Client::new().put("https://eu.tectonic.remarkable.com/sync/v3/root"),
            "/sync/v3/root",
            Some(body.clone()),
            &[
                ("Content-Type", "application/json".into()),
                ("x-goog-hash", crc_header(hash.as_bytes())),
            ],
        )
        .build()
        .unwrap();
        assert_eq!(request.headers().get_all("x-goog-hash").iter().count(), 1);
        assert_eq!(
            request.headers()["x-goog-hash"],
            crc_header(hash.as_bytes())
        );
        assert_ne!(request.headers()["x-goog-hash"], crc_header(&body));
        assert_eq!(request.headers()["content-type"], "application/json");
        let error = HttpError {
            status: 400,
            route: "/sync/v3/files/{hash}".into(),
        };
        assert!(!error.to_string().contains(&hash));
    }
    #[test]
    fn opaque_session_id_is_url_encoded() {
        let p = query(
            "/gentree/v1/PutFile",
            &[("sessionId", "a&/ ?"), ("filePath", "x.pdf")],
        );
        assert!(p.contains("a%26%2F+%3F"));
        assert_eq!(
            Url::parse(&format!("https://{BASE}{p}"))
                .unwrap()
                .query_pairs()
                .count(),
            2
        );
    }
    #[test]
    fn completion_requires_id_generation_hash() {
        let good = json!({"session":{"state":"completed","id":"opaque"},
            "library":{"generation":0,"hash":"ab".repeat(32)}});
        assert_eq!(session(&good).unwrap(), Session::Completed(0));
        for field in ["generation", "hash"] {
            let mut bad = good.clone();
            bad["library"].as_object_mut().unwrap().remove(field);
            assert!(session(&bad).is_err());
        }
        let mut bad = good.clone();
        bad["session"]["id"] = json!("");
        assert!(session(&bad).is_err());
        bad = good;
        bad["library"]["generation"] = json!(-1);
        assert!(session(&bad).is_err());
    }
    #[test]
    fn pending_is_not_completion() {
        assert_eq!(
            session(&json!({"session":{"state":"pending","id":"opaque"}})).unwrap(),
            Session::Pending("opaque".into())
        );
        assert!(session(&json!({"session":{"state":"success","id":"opaque"}})).is_err());
    }
}
