// auth.rs — Sign in with ChatGPT: the OAuth device-code flow the Codex CLI
// uses, and the token refresh that keeps a saved sign-in usable (ported from
// Freebuff sdk/src/chatgpt.ts). Tokens are stored only as a DPAPI blob under
// %APPDATA%\dyland\auth.bin, never plaintext.
//
// Every request here has a fixed destination and never echoes a token
// endpoint body into a user-visible error.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;
use ureq::Agent;

use windows::core::w;
use windows::Win32::Foundation::LocalFree;
use windows::Win32::Security::Cryptography::{
    CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
};

/// The Codex CLI's public installed-application client. Not a secret.
pub const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";

const USERCODE_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/usercode";
const DEVICE_TOKEN_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/token";
const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const DEVICE_REDIRECT_URI: &str = "https://auth.openai.com/deviceauth/callback";

/// Where the user enters the code. Fixed, so a provider answer cannot
/// redirect them.
pub const VERIFICATION_URL: &str = "https://auth.openai.com/codex/device";

/// Sent as `originator`, so OpenAI can tell these requests from the Codex
/// CLI's. The account carries the plan; our client carries the name.
pub const ORIGINATOR: &str = "dyland";
pub const USER_AGENT: &str = "dyland/0.1";

/// Codex model listing client version (OpenAI tailors the list to it).
const MODELS_CLIENT_VERSION: &str = "1.0.0";
/// Refresh this long before the access token expires.
pub const REFRESH_MARGIN_SECS: u64 = 5 * 60;
const AUTH_TIMEOUT: Duration = Duration::from_secs(12);

/// Fallback when the plan's own listing cannot be read (Freebuff's defaults;
/// dyland's default is luna).
pub const DEFAULT_MODEL: &str = "gpt-6-luna";
pub const FALLBACK_MODELS: [&str; 3] = ["gpt-6-sol", "gpt-6-astra", "gpt-6-luna"];

/// What a sign-in saves. Never logged; it is a credential.
#[derive(Clone)]
pub struct Tokens {
    pub access: String,
    pub refresh: String,
    /// Epoch seconds.
    pub expires_at: u64,
    /// The ChatGPT workspace the plan belongs to, sent as `ChatGPT-Account-Id`.
    pub account_id: Option<String>,
}

#[derive(Clone)]
pub struct DeviceLogin {
    pub device_auth_id: String,
    pub user_code: String,
    /// Epoch seconds.
    pub expires_at: u64,
    pub interval_ms: u64,
}

pub enum Poll {
    Pending,
    Connected(Tokens),
}

/// A sign-in failure with a message we wrote, safe to show as is. `expired`
/// means only a new sign-in fixes it; anything else may succeed on retry.
pub struct AuthError {
    pub msg: String,
    pub expired: bool,
}

impl AuthError {
    pub fn new(msg: impl Into<String>, expired: bool) -> Self {
        AuthError { msg: msg.into(), expired }
    }
}

impl std::fmt::Display for AuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.msg)
    }
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn positive_secs(v: Option<&Value>, fallback: u64) -> u64 {
    v.and_then(Value::as_f64).filter(|n| *n > 0.0).map(|n| n as u64).unwrap_or(fallback)
}

/// Minimal base64url decode (no padding) for the JWT payload.
fn b64url(input: &str) -> Option<Vec<u8>> {
    const ALPHA: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = Vec::new();
    let mut buf = 0u32;
    let mut bits = 0u32;
    for &b in input.as_bytes() {
        if b == b'.' || b == b'=' {
            if b == b'=' {
                break;
            }
            continue;
        }
        let v = ALPHA.iter().position(|&c| c == b)? as u32;
        buf = (buf << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
        }
    }
    Some(out)
}

/// The signed-in claims of an access/id token (routing hints only; OpenAI
/// authenticates the token itself).
fn jwt_claims(token: &str) -> Value {
    let payload = token.split('.').nth(1).unwrap_or("");
    serde_json::from_slice(&b64url(payload).unwrap_or_default()).unwrap_or(Value::Null)
}

/// The ChatGPT account id a token names.
pub fn account_id_of(token: &str) -> Option<String> {
    let claims = jwt_claims(token);
    let auth = &claims["https://api.openai.com/auth"];
    let id = auth.get("chatgpt_account_id").or_else(|| claims.get("chatgpt_account_id"));
    id.and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_owned)
}

fn required_str(v: &Value) -> Result<&str, AuthError> {
    v.as_str().filter(|s| !s.is_empty()).ok_or_else(|| {
        AuthError::new("ChatGPT returned an unexpected sign-in response. Try again.", false)
    })
}

fn tokens_from(data: &Value, prev: Option<&Tokens>) -> Result<Tokens, AuthError> {
    let access = required_str(&data["access_token"])?.to_owned();
    let claims = jwt_claims(&access);
    let exp = claims.get("exp").and_then(Value::as_f64);
    let expires_at = match exp {
        Some(e) if e > 0.0 => e as u64,
        _ => now_secs() + positive_secs(data.get("expires_in"), 3600),
    };
    let account_id = account_id_of(&data["id_token"].as_str().unwrap_or(""))
        .or_else(|| account_id_of(&access))
        .or_else(|| prev.and_then(|t| t.account_id.clone()));
    // A refresh may omit the refresh token, which then stays the same.
    let refresh = match data.get("refresh_token") {
        Some(Value::String(s)) if !s.is_empty() => s.clone(),
        _ => prev.map(|t| t.refresh.clone()).filter(|s| !s.is_empty()).ok_or_else(|| {
            AuthError::new("ChatGPT returned an unexpected sign-in response. Try again.", false)
        })?,
    };
    Ok(Tokens { access, refresh, expires_at, account_id })
}

fn agent_http_ok() -> Agent {
    ureq::AgentBuilder::new()
        .timeout(AUTH_TIMEOUT)
        .user_agent(USER_AGENT)
        .build()
}

// ureq distinguishes transport errors from HTTP status errors; we check
// statuses ourselves, so a status error flattens into the response.
fn flatten(r: Result<ureq::Response, ureq::Error>) -> Result<ureq::Response, AuthError> {
    match r {
        Ok(resp) => Ok(resp),
        Err(ureq::Error::Status(_, resp)) => Ok(resp),
        Err(_) => Err(AuthError::new(
            "Could not reach ChatGPT sign-in. Check your connection and try again.",
            false,
        )),
    }
}

fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn form(pairs: &[(&str, &str)]) -> String {
    pairs.iter().map(|(k, v)| format!("{}={}", k, percent_encode(v))).collect::<Vec<_>>().join("&")
}

fn post_json(url: &str, body: &Value) -> Result<ureq::Response, AuthError> {
    flatten(agent_http_ok()
        .post(url)
        .set("content-type", "application/json")
        .set("accept", "application/json")
        .send_string(&body.to_string()))
}

fn post_form(url: &str, body: &str) -> Result<ureq::Response, AuthError> {
    flatten(agent_http_ok()
        .post(url)
        .set("content-type", "application/x-www-form-urlencoded")
        .set("accept", "application/json")
        .send_string(body))
}

fn body_json(resp: ureq::Response) -> Value {
    resp.into_string()
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .unwrap_or(Value::Null)
}

/// Step 1: get a code for the user to enter at the verification URL.
pub fn start_device_login() -> Result<DeviceLogin, AuthError> {
    let resp = post_json(USERCODE_URL, &serde_json::json!({ "client_id": CLIENT_ID }))?;
    if resp.status() != 200 {
        return Err(AuthError::new(
            format!("ChatGPT sign-in could not start (HTTP {}). Try again in a minute.", resp.status()),
            false,
        ));
    }
    let data = body_json(resp);
    Ok(DeviceLogin {
        device_auth_id: required_str(&data["device_auth_id"])?.to_owned(),
        user_code: required_str(&data["user_code"])?.to_owned(),
        expires_at: now_secs() + positive_secs(data.get("expires_in"), 900).min(900),
        interval_ms: positive_secs(data.get("interval"), 5).saturating_mul(1000).max(1000),
    })
}

/// Step 2, repeated every `interval_ms`: `Pending` until the user approves.
pub fn poll_device_login(login: &DeviceLogin) -> Result<Poll, AuthError> {
    if now_secs() >= login.expires_at {
        return Err(AuthError::new("The ChatGPT sign-in code expired. Start again.", true));
    }
    let resp = post_json(
        DEVICE_TOKEN_URL,
        &serde_json::json!({ "device_auth_id": login.device_auth_id, "user_code": login.user_code }),
    )?;
    // Not yet approved.
    if resp.status() == 403 || resp.status() == 404 {
        return Ok(Poll::Pending);
    }
    let status = resp.status();
    let data = body_json(resp);
    if status != 200 {
        match data.get("error").and_then(Value::as_str) {
            Some("authorization_pending") | Some("slow_down") => return Ok(Poll::Pending),
            _ => {
                return Err(AuthError::new(
                    "ChatGPT sign-in was denied or expired. Start again.",
                    true,
                ))
            }
        }
    }
    // The device authorization yields a PKCE code, exchanged here.
    let code = required_str(&data["authorization_code"])?.to_owned();
    let verifier = required_str(&data["code_verifier"])?.to_owned();
    let exchange = post_form(
        TOKEN_URL,
        &form(&[
            ("grant_type", "authorization_code"),
            ("code", &code),
            ("code_verifier", &verifier),
            ("client_id", CLIENT_ID),
            ("redirect_uri", DEVICE_REDIRECT_URI),
        ]),
    )?;
    if exchange.status() != 200 {
        return Err(AuthError::new(
            format!("ChatGPT sign-in could not finish (HTTP {}). Start again.", exchange.status()),
            true,
        ));
    }
    Ok(Poll::Connected(tokens_from(&body_json(exchange), None)?))
}

/// A new access token. OpenAI rotates the refresh token on every use, so only
/// one caller refreshes at a time and must save the answer before anyone else
/// refreshes: a second refresh with the old token fails.
pub fn refresh_tokens(prev: &Tokens) -> Result<Tokens, AuthError> {
    let resp = post_form(
        TOKEN_URL,
        &form(&[
            ("grant_type", "refresh_token"),
            ("client_id", CLIENT_ID),
            ("refresh_token", &prev.refresh),
        ]),
    )?;
    if resp.status() != 200 {
        // 400/401 is a revoked, reused or expired refresh token: only a new
        // sign-in fixes it. Anything else may be transient.
        return Err(AuthError::new(
            "Your ChatGPT sign-in has expired. Connect ChatGPT again from the tray.",
            resp.status() == 400 || resp.status() == 401,
        ));
    }
    tokens_from(&body_json(resp), Some(prev))
}

pub fn needs_refresh(tokens: &Tokens) -> bool {
    tokens.expires_at.saturating_sub(now_secs()) <= REFRESH_MARGIN_SECS
}

/// The plan's code models, in OpenAI's order (`visibility: "list"`, none
/// pending `upgrade`); errors are the caller's fallback path.
pub fn list_models(tokens: &Tokens) -> Result<Vec<String>, String> {
    let url = format!(
        "https://chatgpt.com/backend-api/codex/models?client_version={MODELS_CLIENT_VERSION}"
    );
    let agent = ureq::AgentBuilder::new()
        .timeout(AUTH_TIMEOUT)
        .user_agent(USER_AGENT)
        .build();
    let mut req = agent
        .get(&url)
        .set("accept", "application/json")
        .set("authorization", &format!("Bearer {}", tokens.access))
        .set("originator", ORIGINATOR);
    if let Some(account) = &tokens.account_id {
        req = req.set("chatgpt-account-id", account);
    }
    let resp = match req.call() {
        Ok(r) => r,
        Err(ureq::Error::Status(_, r)) => r,
        Err(_) => return Err("offline".to_owned()),
    };
    if resp.status() != 200 {
        return Err("http".to_owned());
    }
    let data = body_json(resp);
    let mut listed: Vec<(f64, &str)> = data["models"]
        .as_array()
        .map(|a| a.as_slice())
        .unwrap_or(&[])
        .iter()
        .filter_map(|item| {
            let slug = item["slug"].as_str()?;
            if slug.is_empty() || item["visibility"].as_str() != Some("list") {
                return None;
            }
            if matches!(item.get("upgrade"), Some(Value::Bool(true))) {
                return None;
            }
            if !slug
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | ':' | '_' | '-'))
                || slug.len() > 128
            {
                return None;
            }
            Some((item.get("priority").and_then(Value::as_f64).unwrap_or(1.0e9), slug))
        })
        .collect();
    listed.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    Ok(listed.into_iter().map(|(_, s)| s.to_owned()).collect())
}

pub fn pick_model(tokens: &Tokens) -> String {
    list_models(tokens)
        .ok()
        .and_then(|l| l.into_iter().next())
        .unwrap_or_else(|| DEFAULT_MODEL.to_owned())
}

// ---------------------------------------------------------------------------
// token store: DPAPI blob at %APPDATA%\dyland\auth.bin — never plaintext
// ---------------------------------------------------------------------------

fn store_path() -> Option<std::path::PathBuf> {
    std::env::var_os("APPDATA").map(|base| std::path::Path::new(&base).join("dyland").join("auth.bin"))
}

unsafe fn dpapi(data: &[u8], protect: bool) -> Result<Vec<u8>, String> {
    let mut out = CRYPT_INTEGER_BLOB::default();
    let input = CRYPT_INTEGER_BLOB {
        cbData: data.len() as u32,
        pbData: data.as_ptr() as *mut u8,
    };
    let res = if protect {
        CryptProtectData(&input, w!("dyland"), None, None, None, CRYPTPROTECT_UI_FORBIDDEN, &mut out)
    } else {
        CryptUnprotectData(&input, None, None, None, None, CRYPTPROTECT_UI_FORBIDDEN, &mut out)
    };
    if res.is_err() {
        return Err("dpapi".into());
    }
    let blob = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
    let _ = LocalFree(windows::Win32::Foundation::HLOCAL(out.pbData as *mut _));
    Ok(blob)
}

pub fn save_tokens(tokens: &Tokens) -> Result<(), String> {
    let plain = serde_json::json!({
        "v": 1,
        "accessToken": tokens.access,
        "refreshToken": tokens.refresh,
        "expiresAt": tokens.expires_at,
        "accountId": tokens.account_id,
    })
    .to_string();
    let blob = unsafe { dpapi(plain.as_bytes(), true) }?;
    let path = store_path().ok_or("no %APPDATA%")?;
    std::fs::create_dir_all(path.parent().unwrap()).map_err(|_| "mkdir".to_owned())?;
    let tmp = path.with_extension("bin.tmp");
    std::fs::write(&tmp, &blob).map_err(|_| "write".to_owned())?;
    std::fs::rename(&tmp, &path).map_err(|_| "rename".to_owned())
}

pub fn load_tokens() -> Option<Tokens> {
    let blob = std::fs::read(store_path()?).ok()?;
    let plain = unsafe { dpapi(&blob, false) }.ok()?;
    let value = serde_json::from_slice::<Value>(&plain).ok()?;
    if value.get("v")?.as_i64()? != 1 {
        return None;
    }
    // store format (camelCase) — tokens_from parses the OAuth wire format
    Some(Tokens {
        access: value.get("accessToken")?.as_str()?.to_owned(),
        refresh: value.get("refreshToken")?.as_str()?.to_owned(),
        expires_at: value.get("expiresAt")?.as_u64()?,
        account_id: value.get("accountId").and_then(Value::as_str).map(str::to_owned),
    })
}

pub fn delete_tokens() {
    if let Some(path) = store_path() {
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn b64url_and_claims_roundtrip() {
        // known vector: {"exp":1}
        assert_eq!(b64url("eyJleHAiOjF9").unwrap(), b"{\"exp\":1}");

        // account id lookup from a fabricated token
        let payload = r#"{"exp":1234567890,"https://api.openai.com/auth":{"chatgpt_account_id":"acc_123"}}"#;
        let token = format!("x.{}.y", encode_b64url(payload.as_bytes()));
        assert_eq!(account_id_of(&token).as_deref(), Some("acc_123"));
    }

    // test-only base64url encode (no padding)
    fn encode_b64url(data: &[u8]) -> String {
        const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let mut out = String::new();
        for chunk in data.chunks(3) {
            let n = (chunk[0] as u32) << 16
                | (*chunk.get(1).unwrap_or(&0) as u32) << 8
                | (*chunk.get(2).unwrap_or(&0) as u32);
            out.push(A[(n >> 18) as usize & 63] as char);
            out.push(A[(n >> 12) as usize & 63] as char);
            if chunk.len() > 1 {
                out.push(A[(n >> 6) as usize & 63] as char);
            }
            if chunk.len() > 2 {
                out.push(A[n as usize & 63] as char);
            }
        }
        out
    }

    #[test]
    fn percent_encode_escapes_jwt_characters() {
        assert_eq!(percent_encode("abc-_~.0"), "abc-_~.0");
        assert_eq!(percent_encode("a b&c+d/e"), "a%20b%26c%2Bd%2Fe");
    }
}
