//! Independent app authentication, using the request contracts in CLIENT-AUTH.md.
//! JWT claims below are unverified routing hints, never identity verification.

use anyhow::{anyhow, bail, Context, Result};
use base64::{engine::general_purpose, Engine as _};
use serde::Serialize;
use serde_json::Value;
use std::io::IsTerminal;
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;
use zeroize::Zeroizing;

const BASE: &str = "internal.cloud.remarkable.com";
const DISCOVERY: &str = "/discovery/v1/endpoints";
const REGISTER: &str = "/token/json/2/device/new";
const EXCHANGE: &str = "/token/json/2/user/new";
const TOKEN_LIMIT: usize = 32_768;
const SERVICE: &str = "remarkable-send";
const ACCOUNT: &str = "device-token";
const USER_ACCOUNT: &str = "user-token";
const CACHE_EXPIRY_MARGIN_SECONDS: f64 = 300.0;

fn valid_label(label: &str) -> bool {
    !label.is_empty()
        && label.len() <= 63
        && label
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
        && label.as_bytes()[0].is_ascii_alphanumeric()
        && label.as_bytes()[label.len() - 1].is_ascii_alphanumeric()
}

/// Validate only an HTTPS hostname, with no port, path, userinfo or IP literal.
pub fn vendor_host(host: &str) -> Result<()> {
    if host.len() > 253
        || !host.split('.').all(valid_label)
        || !(host.ends_with(".remarkable.com") || host.ends_with(".cloud.remarkable.engineering"))
    {
        bail!("Unexpected vendor hostname");
    }
    Ok(())
}

fn token_text(raw: &[u8]) -> Result<String> {
    if raw.is_empty() || raw.len() > TOKEN_LIMIT || !raw.is_ascii() {
        bail!("Unexpected token response format");
    }
    let text = std::str::from_utf8(raw)
        .map_err(|_| anyhow!("Unexpected token response format"))?
        .trim();
    if text.is_empty() || !text.bytes().all(|c| c.is_ascii_graphic()) {
        bail!("Unexpected token response format");
    }
    Ok(text.to_owned())
}

fn validate_code(code: &str) -> Result<()> {
    if code.len() != 8 || !code.bytes().all(|c| c.is_ascii_alphanumeric()) {
        bail!("Expected an eight-character pairing code");
    }
    Ok(())
}

fn routing_host_at(token: &str, now: f64) -> Result<String> {
    // Validate without including any token/claim bytes in diagnostics.
    if token.is_empty() || token.len() > TOKEN_LIMIT || !token.bytes().all(|c| c.is_ascii_graphic())
    {
        bail!("Unexpected user-token representation");
    }
    let parts: Vec<_> = token.split('.').collect();
    if parts.len() != 3 || parts.iter().any(|part| part.is_empty()) {
        bail!("Unexpected user-token representation");
    }
    let payload = Zeroizing::new(
        general_purpose::URL_SAFE_NO_PAD
            .decode(parts[1])
            .or_else(|_| general_purpose::URL_SAFE.decode(parts[1]))
            .map_err(|_| anyhow!("Unexpected user-token payload"))?,
    );
    let claims: Value =
        serde_json::from_slice(&payload).map_err(|_| anyhow!("Unexpected user-token payload"))?;
    let claims = claims
        .as_object()
        .ok_or_else(|| anyhow!("Unexpected user-token payload"))?;
    let expiry = claims
        .get("exp")
        .and_then(Value::as_f64)
        .filter(|value| value.is_finite())
        .ok_or_else(|| anyhow!("User token lacks a usable expiry"))?;
    if !now.is_finite() || expiry <= now + 30.0 {
        bail!("User token is expired or too close to expiry");
    }
    let primary = claims.get("https://auth.remarkable.com/tectonic");
    let hint = match primary {
        None | Some(Value::Null) => claims.get("tectonic"),
        Some(Value::String(value)) if value.is_empty() => claims.get("tectonic"),
        value => value,
    };
    let region = match hint {
        None | Some(Value::Null) => return Ok(BASE.to_owned()),
        Some(Value::String(value)) if value.is_empty() => return Ok(BASE.to_owned()),
        Some(Value::String(value)) if valid_label(value) => value,
        _ => bail!("Unexpected routing region"),
    };
    let host = format!("{region}.tectonic.remarkable.com");
    vendor_host(&host)?;
    Ok(host)
}

pub fn routing_host(token: &str) -> Result<String> {
    routing_host_at(token, current_time()?)
}

fn current_time() -> Result<f64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| anyhow!("System clock is before the Unix epoch"))?
        .as_secs_f64())
}

fn credential_entry(account: &str) -> Result<keyring::Entry> {
    // keyring's fallback on unsupported platforms can be a mock store. Never
    // allow it to pretend that a freshly registered credential was persisted.
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        bail!("Secure credential storage is unavailable on this platform");
    }
    #[cfg(any(windows, target_os = "linux"))]
    {
        keyring::Entry::new(SERVICE, account)
            .map_err(|_| anyhow!("Could not open the system credential store"))
    }
}

/// Report whether this application has a saved device credential.
///
/// A missing credential is `Ok(false)`. A locked, unavailable, or otherwise
/// broken credential store is an error so the UI never presents it as an
/// unpaired account.
pub fn has_credential() -> Result<bool> {
    let entry = credential_entry(ACCOUNT)?;
    match entry.get_password() {
        Ok(value) => {
            let _value = Zeroizing::new(value);
            Ok(true)
        }
        Err(keyring::Error::NoEntry) => Ok(false),
        Err(_) => bail!("Could not read the system credential store"),
    }
}

// Called only after the paired device credential has been read and validated.
// An unusable cached token is refreshable; a storage failure is not absence.
fn cached_user(read: keyring::Result<String>, now: f64) -> Result<Option<(String, String)>> {
    if !now.is_finite() {
        bail!("System clock is unusable");
    }
    match read {
        Ok(value) => {
            let token = Zeroizing::new(value);
            // Routing validation already reserves 30 seconds. Cached reuse
            // reserves five minutes to leave time for a subsequent upload.
            Ok(
                routing_host_at(&token, now + CACHE_EXPIRY_MARGIN_SECONDS - 30.0)
                    .ok()
                    .map(|host| (token.to_string(), host)),
            )
        }
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(_) => bail!("Could not read the cached user token from the system credential store"),
    }
}

fn clear_cached_user(entry: &keyring::Entry) -> Result<()> {
    match entry.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => {}
        Err(_) => bail!("New client registration succeeded, but the old user token could not be cleared; the saved device credential was not changed"),
    }
    match entry.get_password() {
        Err(keyring::Error::NoEntry) => Ok(()),
        Ok(value) => {
            let _value = Zeroizing::new(value);
            bail!("The old user token remained after clearing; the saved device credential was not changed")
        }
        Err(_) => bail!(
            "Could not verify user-token removal; the saved device credential was not changed"
        ),
    }
}

fn save_cached_user(entry: &keyring::Entry, user: &str) -> Result<()> {
    entry.set_password(user).map_err(|_| {
        anyhow!("Authentication succeeded, but its user token could not be saved in the system credential store")
    })?;
    let saved = Zeroizing::new(
        entry
            .get_password()
            .map_err(|_| anyhow!("User-token write completed, but reading it back failed"))?,
    );
    if saved.as_str() != user {
        bail!("Saved user-token read-back did not match");
    }
    Ok(())
}

fn discovery_host(http: &crate::cloud::Http) -> Result<String> {
    let response = http.request(BASE, "GET", DISCOVERY, None, None, &[], 65_536)?;
    let discovery: Value =
        serde_json::from_slice(&response).map_err(|_| anyhow!("Unexpected discovery response"))?;
    let host = discovery
        .as_object()
        .and_then(|object| object.get("webapp"))
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("Discovery did not provide an authentication host"))?;
    vendor_host(host)?;
    Ok(host.to_owned())
}

fn exchange(http: &crate::cloud::Http, host: &str, device: &str) -> Result<String> {
    let raw = Zeroizing::new(http.request(
        host,
        "POST",
        EXCHANGE,
        Some(device),
        Some(Vec::new()),
        &[],
        TOKEN_LIMIT,
    )?);
    token_text(&raw)
}

fn pair_with_code_inner(code: &str, replace: bool) -> Result<(crate::cloud::Http, String, String)> {
    validate_code(code)?;
    let entry = credential_entry(ACCOUNT)?;
    let user_entry = credential_entry(USER_ACCOUNT)?;
    match entry.get_password() {
        Ok(existing) => {
            let _existing = Zeroizing::new(existing);
            if !replace {
                bail!("A TurboRM credential already exists. Reuse it, or run pair --replace to deliberately register a replacement using a fresh code.");
            }
        }
        Err(keyring::Error::NoEntry) => {}
        Err(_) => bail!("Could not check the system credential store; no pairing was attempted"),
    }
    let http = crate::cloud::Http::new()?;
    let host = discovery_host(&http)?;
    #[derive(Serialize)]
    struct Registration<'a> {
        code: &'a str,
        #[serde(rename = "deviceDesc")]
        device_desc: &'static str,
        #[serde(rename = "deviceID")]
        device_id: String,
    }
    let body = serde_json::to_vec(&Registration {
        code,
        device_desc: "desktop-linux",
        device_id: Uuid::new_v4().to_string(),
    })
    .map_err(|_| anyhow!("Could not build the registration request"))?;
    let raw = Zeroizing::new(http.request(
        &host,
        "POST",
        REGISTER,
        None,
        Some(body),
        &[],
        TOKEN_LIMIT,
    )?);
    let device = Zeroizing::new(token_text(&raw)?);
    // Do not touch either saved token before registration returned valid data.
    // Clear the old user token before changing accounts: a partial failure may
    // require renewal, but must never reuse another account's cached token.
    clear_cached_user(&user_entry)?;
    // Persist immediately: a later exchange/network failure must not lose the
    // already registered application credential or silently register another.
    entry.set_password(&device).map_err(|_| {
        anyhow!("The client was registered, but its credential could not be saved in the system credential store. No user-token exchange was attempted.")
    })?;
    let saved = Zeroizing::new(entry.get_password().map_err(|_| {
        anyhow!("Credential write completed, but reading it back failed; no user-token exchange was attempted")
    })?);
    if saved.as_str() != device.as_str() {
        bail!("Credential read-back did not match; no user-token exchange was attempted");
    }
    let user = Zeroizing::new(exchange(&http, &host, &device).context(
        "Client credential is saved. User-token exchange failed; reuse the saved credential on the next run",
    )?);
    let cloud = routing_host(&user).context(
        "Client credential is saved, but the returned user token was unusable; pairing was not repeated",
    )?;
    save_cached_user(&user_entry, &user)?;
    Ok((http, user.to_string(), cloud))
}

/// Pair using a code supplied by a UI and persist credentials securely.
///
/// This function never writes to stdout or stderr. The caller owns all user
/// interaction and must obtain a fresh code from the reMarkable pairing page.
pub fn pair_with_code(code: &str, replace: bool) -> Result<()> {
    let code = Zeroizing::new(code.trim().to_owned());
    pair_with_code_inner(&code, replace).map(|(_, user, _)| {
        let _user = Zeroizing::new(user);
    })
}

/// Register a software client from an interactive terminal.
pub fn pair(replace: bool, diagnose: bool) -> Result<()> {
    if !std::io::stdin().is_terminal() {
        bail!("Pairing requires an interactive terminal; do not pass pairing codes as arguments");
    }
    eprintln!("Get a fresh code at https://my.remarkable.com/device/browser/connect");
    let code = Zeroizing::new(
        rpassword::prompt_password("Pairing code (hidden): ")
            .map_err(|_| anyhow!("Could not read a hidden pairing code from the terminal"))?,
    );
    let (http, user, cloud) = pair_with_code_inner(code.trim(), replace)?;
    println!("Paired. Client and user credentials are saved in the system credential store.");
    if diagnose {
        http.diagnose(&cloud, &user)?;
    }
    Ok(())
}

/// Reuse a usable protected user token, otherwise renew with the saved device.
pub fn connect() -> Result<(crate::cloud::Http, String, String)> {
    let entry = credential_entry(ACCOUNT)?;
    let device = Zeroizing::new(match entry.get_password() {
        Ok(value) => value,
        Err(keyring::Error::NoEntry) => {
            bail!("No TurboRM credential exists. Run the pair command first")
        }
        Err(_) => bail!("Could not read the client credential from the system credential store"),
    });
    let normalized = Zeroizing::new(token_text(device.as_bytes())?);
    if normalized.as_str() != device.as_str() {
        bail!("Saved client credential has an unexpected format");
    }
    let http = crate::cloud::Http::new()?;
    let user_entry = credential_entry(USER_ACCOUNT)?;
    if let Some((user, cloud)) = cached_user(user_entry.get_password(), current_time()?)? {
        return Ok((http, user, cloud));
    }
    let host = discovery_host(&http)?;
    let user = Zeroizing::new(
        exchange(&http, &host, &device)
            .context("Could not renew authentication; the saved client credential was retained")?,
    );
    let cloud = routing_host(&user)?;
    save_cached_user(&user_entry, &user)?;
    Ok((http, user.to_string(), cloud))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn jwt(claims: Value) -> String {
        format!(
            "e30.{}.signature",
            general_purpose::URL_SAFE_NO_PAD.encode(claims.to_string())
        )
    }

    #[test]
    fn host_validation_rejects_credential_and_url_injection() {
        for host in [
            BASE,
            "eu.tectonic.remarkable.com",
            "webapp-prod.cloud.remarkable.engineering",
        ] {
            assert!(vendor_host(host).is_ok());
        }
        for host in [
            "remarkable.com",
            "evilremarkable.com",
            "eu.tectonic.remarkable.com.evil.test",
            "eu.tectonic.remarkable.com:443",
            "user@eu.remarkable.com",
            "https://eu.remarkable.com",
            "eu.remarkable.com/",
            "eu..remarkable.com",
            "-eu.remarkable.com",
            "eu-.remarkable.com",
            "EU.remarkable.com",
            "127.0.0.1",
            "eu.remarkable.com\r\nX: bad",
        ] {
            assert!(vendor_host(host).is_err(), "accepted {host:?}");
        }
        assert!(vendor_host(&format!("{}.remarkable.com", "a".repeat(64))).is_err());
    }

    #[test]
    fn routes_only_usable_tokens_to_vendor_region() {
        assert_eq!(
            routing_host_at(&jwt(json!({"exp": 200})), 100.0).unwrap(),
            BASE
        );
        assert_eq!(
            routing_host_at(&jwt(json!({"exp": 200,"tectonic":"eu"})), 100.0).unwrap(),
            "eu.tectonic.remarkable.com"
        );
        assert_eq!(
            routing_host_at(
                &jwt(
                    json!({"exp": 200,"https://auth.remarkable.com/tectonic":"us","tectonic":"eu"})
                ),
                100.0
            )
            .unwrap(),
            "us.tectonic.remarkable.com"
        );
        assert_eq!(
            routing_host_at(
                &jwt(json!({"exp": 200,"https://auth.remarkable.com/tectonic":"","tectonic":"eu"})),
                100.0
            )
            .unwrap(),
            "eu.tectonic.remarkable.com"
        );
        for claims in [
            json!({}),
            json!({"exp":130}),
            json!({"exp":"200"}),
            json!({"exp":true}),
            json!({"exp":200,"tectonic":"eu/evil"}),
            json!({"exp":200,"tectonic":false}),
            json!([]),
        ] {
            assert!(routing_host_at(&jwt(claims), 100.0).is_err());
        }
    }

    #[test]
    fn token_validation_bounds_and_redacts() {
        assert_eq!(token_text(b"token\r\n").unwrap(), "token");
        for raw in [b"".as_slice(), b" ", b"a b", b"x\r\nInjected", b"\xff"] {
            assert!(token_text(raw).is_err());
        }
        assert!(token_text(&vec![b'a'; TOKEN_LIMIT + 1]).is_err());
        for token in ["", "a.b", "a.b.c.d", "a.!.secret", "a..c", "a.b.c\n"] {
            let err = routing_host_at(token, 100.0).unwrap_err().to_string();
            assert!(!err.contains("secret"));
        }
    }

    #[test]
    fn pairing_codes_are_ascii_alphanumeric_only() {
        assert!(validate_code("aB12Cd34").is_ok());
        for code in [
            "",
            "1234567",
            "123456789",
            "abcd ef1",
            "abcd\nef1",
            "abcdefghé",
        ] {
            assert!(validate_code(code).is_err());
        }
    }

    #[test]
    fn ui_pairing_rejects_invalid_code_before_store_or_network_access() {
        let error = pair_with_code("private-code", false)
            .unwrap_err()
            .to_string();
        assert_eq!(error, "Expected an eight-character pairing code");
        assert!(!error.contains("private-code"));
    }

    #[test]
    fn cached_token_reuses_only_valid_route_with_five_minute_margin() {
        let token = jwt(json!({"exp": 401,"tectonic":"eu"}));
        assert_eq!(
            cached_user(Ok(token.clone()), 100.0).unwrap(),
            Some((token, "eu.tectonic.remarkable.com".to_owned()))
        );
        for token in [
            jwt(json!({"exp": 400,"tectonic":"eu"})),
            jwt(json!({"exp": 130,"tectonic":"eu"})),
            jwt(json!({"exp": 99,"tectonic":"eu"})),
            jwt(json!({"exp": 500,"tectonic":"eu/secret"})),
            jwt(json!({"exp": "500"})),
            "malformed-secret".to_owned(),
        ] {
            assert!(cached_user(Ok(token), 100.0).unwrap().is_none());
        }
        assert!(cached_user(Err(keyring::Error::NoEntry), 100.0)
            .unwrap()
            .is_none());
    }

    #[test]
    fn cached_token_storage_errors_are_not_absence_and_are_redacted() {
        for error in [
            keyring::Error::NoStorageAccess(Box::new(std::io::Error::other("private-detail"))),
            keyring::Error::PlatformFailure(Box::new(std::io::Error::other("private-detail"))),
            keyring::Error::Invalid("private-detail".into(), "private-detail".into()),
            keyring::Error::BadEncoding(b"private-detail".to_vec()),
        ] {
            let message = cached_user(Err(error), 100.0).unwrap_err().to_string();
            assert!(!message.contains("private-detail"));
        }
        assert!(cached_user(Err(keyring::Error::NoEntry), f64::NAN).is_err());
    }
}
