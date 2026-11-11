// auth.rs — Sign in with ChatGPT (documented open-source token-sharing flow):
// Authorization Code + PKCE against a 127.0.0.1 loopback callback, ID-token
// validation via OpenAI's published JWKS, and a plan-scope gate before any
// inference. No Codex or backend-api endpoints, no API keys, no secrets.
//
// Flow: begin_authorize() -> open the returned URL in the system browser ->
// wait_callback() -> exchange_code() -> finish_signin(). Refresh uses the
// standard refresh-token grant. Tokens persist only as a DPAPI blob at
// %APPDATA%\notchbud\auth.bin, never plaintext.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;
use ureq::Agent;

use windows::core::w;
use windows::Win32::Foundation::{LocalFree, STATUS_SUCCESS};
use windows::Win32::Security::Cryptography::{
    BCryptGenRandom, CryptProtectData, CryptUnprotectData, BCRYPT_USE_SYSTEM_PREFERRED_RNG,
    CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
};

/// First-time registration entrypoint. Never saved or used for token exchange.
const DYNAMIC_CLIENT_ID: &str = "dynamic_agent_client";
/// This app's actual name, sent only on initial dynamic registration.
const AGENT_NAME_HINT: &str = "notchbud";

const AUTHORIZE_URL: &str = "https://auth.openai.com/api/accounts/authorize";
const TOKEN_URL: &str = "https://auth.openai.com/api/accounts/oauth/token";
const REVOCATION_URL: &str = "https://auth.openai.com/api/accounts/oauth/revoke";
const JWKS_URL: &str = "https://auth.openai.com/.well-known/jwks.json";
const ISSUER: &str = "https://auth.openai.com";
const RESOURCE: &str = "https://api.openai.com/v1";
const MODELS_URL: &str = "https://api.openai.com/v1/models";

/// Identity scopes plus the ChatGPT-plan usage scopes.
const SCOPES: &str =
    "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct";
/// The scope that authorizes ChatGPT-plan inference. A valid sign-in without
/// it is identity-only: signed in, but inference stays disabled.
pub const PLAN_SCOPE: &str = "chatgpt.tokens.use.direct";

pub const USER_AGENT: &str = "notchbud/0.1";

/// Refresh this long before the access token expires (tokens live one hour).
pub const REFRESH_MARGIN_SECS: u64 = 5 * 60;
const AUTH_TIMEOUT: Duration = Duration::from_secs(12);
/// How long the loopback callback waits for the browser round-trip.
const CALLBACK_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const CALLBACK_PATH: &str = "/auth/callback";
/// Small clock tolerance for ID-token expiry checks.
const CLOCK_SKEW_SECS: u64 = 60;

/// What a sign-in saves. Never logged; it is a credential.
#[derive(Clone)]
pub struct Tokens {
    pub access: String,
    pub refresh: String,
    pub id_token: String,
    /// Epoch seconds.
    pub expires_at: u64,
    /// The issued OAuth client ID from registration (`oaiapp_...`), bound to
    /// this ChatGPT account. Never `dynamic_agent_client`.
    pub client_id: String,
    /// Validated ID-token `sub`: the stable account identity.
    pub subject: String,
    pub email: Option<String>,
    /// Display name from the OpenAI ID token (`name`, else given/family,
    /// else preferred_username/nickname). Used for the greeting, the model
    /// instructions and Settings — never a credential, never typed by hand.
    pub name: Option<String>,
    /// Granted scopes from the token response.
    pub scopes: Vec<String>,
    /// True when the grant includes `chatgpt.tokens.use.direct`.
    pub plan_allowed: bool,
    /// This host's stable `ext_agent_host_id`.
    pub host_id: String,
}

impl Tokens {
    pub fn plan_ready(&self) -> bool {
        self.plan_allowed && !self.access.is_empty() && !self.refresh.is_empty()
    }
}

/// A sign-in failure with a message we wrote, safe to show as is. `expired`
/// means only a new sign-in fixes it; anything else may succeed on retry.
pub struct AuthError {
    pub msg: String,
    pub expired: bool,
}

impl AuthError {
    pub fn new(msg: impl Into<String>, expired: bool) -> Self {
        AuthError {
            msg: msg.into(),
            expired,
        }
    }
}

impl std::fmt::Display for AuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.msg)
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn positive_secs(v: Option<&Value>, fallback: u64) -> u64 {
    v.and_then(Value::as_f64)
        .filter(|n| *n > 0.0)
        .map(|n| n as u64)
        .unwrap_or(fallback)
}

// ---------------------------------------------------------------------------
// randomness, base64url, PKCE
// ---------------------------------------------------------------------------

fn rand_bytes(n: usize) -> Result<Vec<u8>, AuthError> {
    let mut buf = vec![0u8; n];
    let status =
        unsafe { BCryptGenRandom(None, buf.as_mut_slice(), BCRYPT_USE_SYSTEM_PREFERRED_RNG) };
    if status == STATUS_SUCCESS {
        Ok(buf)
    } else {
        Err(AuthError::new(
            "ChatGPT sign-in could not start. Try again.",
            false,
        ))
    }
}

fn b64url_encode(data: &[u8]) -> String {
    const ALPHA: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let n = (chunk[0] as u32) << 16
            | (*chunk.get(1).unwrap_or(&0) as u32) << 8
            | (*chunk.get(2).unwrap_or(&0) as u32);
        out.push(ALPHA[(n >> 18) as usize & 63] as char);
        out.push(ALPHA[(n >> 12) as usize & 63] as char);
        if chunk.len() > 1 {
            out.push(ALPHA[(n >> 6) as usize & 63] as char);
        }
        if chunk.len() > 2 {
            out.push(ALPHA[n as usize & 63] as char);
        }
    }
    out
}

/// Minimal base64url decode (no padding) for JWT parts and JWK integers.
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

fn pkce_challenge(verifier: &str) -> String {
    use sha2::{Digest, Sha256};
    b64url_encode(&Sha256::digest(verifier.as_bytes()))
}

// ---------------------------------------------------------------------------
// stable host id (urn:uuid:...) — one per machine, persisted before first use
// ---------------------------------------------------------------------------

fn host_path() -> Option<std::path::PathBuf> {
    std::env::var_os("APPDATA")
        .map(|base| std::path::Path::new(&base).join("notchbud").join("host_id"))
}

fn format_uuid(r: &[u8]) -> String {
    format!(
        "urn:uuid:{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        r[0], r[1], r[2], r[3], r[4], r[5], r[6], r[7],
        r[8], r[9], r[10], r[11], r[12], r[13], r[14], r[15]
    )
}

fn valid_host_id(s: &str) -> bool {
    s.len() == 45
        && s.starts_with("urn:uuid:")
        && s[9..].chars().all(|c| c.is_ascii_hexdigit() || c == '-')
}

/// Load this host's `ext_agent_host_id`, creating and persisting a UUIDv4 on
/// first use. An opaque identifier, not a credential.
pub fn load_or_create_host_id() -> Result<String, AuthError> {
    if let Some(path) = host_path() {
        if let Ok(s) = std::fs::read_to_string(&path) {
            let s = s.trim().to_owned();
            if valid_host_id(&s) {
                return Ok(s);
            }
        }
        let mut r = rand_bytes(16)?;
        // UUIDv4 version + variant bits.
        r[6] = (r[6] & 0x0f) | 0x40;
        r[8] = (r[8] & 0x3f) | 0x80;
        let id = format_uuid(&r);
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        // Best effort: a regenerated id next launch is still a valid host id.
        let _ = std::fs::write(&path, &id);
        return Ok(id);
    }
    Err(AuthError::new(
        "ChatGPT sign-in could not start. Try again.",
        false,
    ))
}

// ---------------------------------------------------------------------------
// authorization: loopback listener + browser URL
// ---------------------------------------------------------------------------

pub struct PendingAuth {
    pub listener: TcpListener,
    pub redirect_uri: String,
    pub state: String,
    pub nonce: String,
    pub verifier: String,
    /// The client id to exchange with: the saved issued id for reauth, or
    /// None for a fresh registration (the callback supplies the issued id).
    pub client_id: Option<String>,
    pub deadline: std::time::Instant,
}

pub struct Callback {
    pub code: String,
    pub client_id: String,
}

/// Start the loopback listener, mint fresh state/nonce/PKCE, and build the
/// browser URL. For a previously registered account the saved issued client id
/// (plus id/email hints) is used; otherwise this is a dynamic registration.
pub fn begin_authorize(
    saved: Option<&Tokens>,
    host_id: &str,
) -> Result<(PendingAuth, String), AuthError> {
    let listener = TcpListener::bind("127.0.0.1:0")
        .map_err(|_| AuthError::new("ChatGPT sign-in could not start. Try again.", false))?;
    let port = listener
        .local_addr()
        .map_err(|_| AuthError::new("ChatGPT sign-in could not start. Try again.", false))?
        .port();
    let redirect_uri = format!("http://127.0.0.1:{port}{CALLBACK_PATH}");
    let state = b64url_encode(&rand_bytes(24)?);
    let nonce = b64url_encode(&rand_bytes(24)?);
    let verifier = b64url_encode(&rand_bytes(48)?);
    let challenge = pkce_challenge(&verifier);

    let mut url = format!(
        "{AUTHORIZE_URL}?response_type=code&redirect_uri={}&scope={}&resource={}&state={}&nonce={}&code_challenge={}&code_challenge_method=S256&ext_agent_host_id={}",
        percent_encode(&redirect_uri),
        percent_encode(SCOPES),
        percent_encode(RESOURCE),
        percent_encode(&state),
        percent_encode(&nonce),
        percent_encode(&challenge),
        percent_encode(host_id),
    );
    let client_id = if let Some(prev) = saved {
        url.push_str(&format!("&client_id={}", percent_encode(&prev.client_id)));
        if !prev.id_token.is_empty() {
            // The hint may be expired; it identifies the account only. It is
            // sent to OpenAI alone — never logged (see wait_callback).
            url.push_str(&format!(
                "&id_token_hint={}",
                percent_encode(&prev.id_token)
            ));
        }
        if let Some(email) = &prev.email {
            if !email.is_empty() {
                url.push_str(&format!("&login_hint={}", percent_encode(email)));
            }
        }
        Some(prev.client_id.clone())
    } else {
        url.push_str(&format!(
            "&client_id={DYNAMIC_CLIENT_ID}&agent_name_hint={}",
            percent_encode(AGENT_NAME_HINT)
        ));
        None
    };

    Ok((
        PendingAuth {
            listener,
            redirect_uri,
            state,
            nonce,
            verifier,
            client_id,
            deadline: std::time::Instant::now() + CALLBACK_TIMEOUT,
        },
        url,
    ))
}

fn percent_decode(s: &str) -> String {
    let mut out = Vec::with_capacity(s.len());
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => {
                let hi = (b[i + 1] as char).to_digit(16);
                let lo = (b[i + 2] as char).to_digit(16);
                match (hi, lo) {
                    (Some(h), Some(l)) => {
                        out.push((h * 16 + l) as u8);
                        i += 3;
                    }
                    _ => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn query_params(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter_map(|pair| {
            let (k, v) = pair.split_once('=')?;
            Some((percent_decode(k), percent_decode(v)))
        })
        .collect()
}

fn respond(stream: &mut std::net::TcpStream, status: &str, title: &str, body: &str) {
    let page = format!(
        "<!doctype html><html><body style=\"font-family:sans-serif;padding:40px\">\
         <h2>{title}</h2><p>{body}</p></body></html>"
    );
    let _ = stream.write_all(
        format!(
            "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{page}",
            page.len()
        )
        .as_bytes(),
    );
}

fn read_request(stream: &mut std::net::TcpStream) -> Option<String> {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let mut buf = Vec::with_capacity(2048);
    let mut chunk = [0u8; 1024];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if buf.len() > 32 * 1024 {
                    return None;
                }
                if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            Err(_) => return None,
        }
    }
    String::from_utf8(buf).ok()
}

/// Block until the browser returns to the loopback callback. Validates `state`
/// before trusting anything else; surfaces OAuth declines as errors. Query
/// values (which may carry tokens) are never logged. `cancelled` is polled
/// every loop turn so the island's Cancel link aborts promptly.
pub fn wait_callback(
    pending: PendingAuth,
    cancelled: &AtomicBool,
) -> Result<(Callback, PendingAuthMeta), AuthError> {
    let PendingAuth {
        listener,
        redirect_uri,
        state,
        nonce,
        verifier,
        client_id,
        deadline,
    } = pending;
    let _ = listener.set_nonblocking(true);
    loop {
        if cancelled.load(Ordering::Relaxed) {
            return Err(AuthError::new("Sign-in cancelled.", false));
        }
        if std::time::Instant::now() >= deadline {
            return Err(AuthError::new(
                "ChatGPT sign-in timed out. Connect again from the tray.",
                false,
            ));
        }
        let (mut stream, _) = match listener.accept() {
            Ok(pair) => pair,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
            Err(_) => {
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
        };
        let Some(req) = read_request(&mut stream) else {
            continue;
        };
        let mut words = req.lines().next().unwrap_or("").split_whitespace();
        let method = words.next().unwrap_or("");
        let target = words.next().unwrap_or("");
        if method != "GET" {
            respond(
                &mut stream,
                "405 Method Not Allowed",
                "Notchbud",
                "That request is not supported.",
            );
            continue;
        }
        let (path, query) = match target.split_once('?') {
            Some((p, q)) => (p, q),
            None => (target, ""),
        };
        if path != CALLBACK_PATH {
            respond(
                &mut stream,
                "404 Not Found",
                "Notchbud",
                "Unknown callback path.",
            );
            continue;
        }
        let params = query_params(query);
        let get = |k: &str| {
            params
                .iter()
                .find(|(key, _)| key == k)
                .map(|(_, v)| v.clone())
        };
        // State first: an unverified callback is never redeemed.
        if get("state").as_deref() != Some(state.as_str()) {
            respond(
                &mut stream,
                "400 Bad Request",
                "Notchbud",
                "This sign-in attempt could not be verified. Start again from the app.",
            );
            return Err(AuthError::new(
                "Verification failed. Start again.",
                false,
            ));
        }
        if let Some(err) = get("error") {
            respond(
                &mut stream,
                "200 OK",
                "Notchbud",
                "Sign-in was not completed. You can close this page and try again from the app.",
            );
            if err == "access_denied" {
                return Err(AuthError::new(
                    "Sign-in declined. Connect again.",
                    false,
                ));
            }
            return Err(AuthError::new(
                "Sign-in failed. Try again.",
                false,
            ));
        }
        let Some(code) = get("code").filter(|c| !c.is_empty()) else {
            respond(
                &mut stream,
                "400 Bad Request",
                "Notchbud",
                "The callback did not contain an authorization code.",
            );
            return Err(AuthError::new(
                "No code returned. Start again.",
                false,
            ));
        };
        // Resolve the issued client id: fresh registrations must supply one
        // (never the dynamic entrypoint); reauth may omit it, in which case
        // the saved id stands. A different id is rejected, not adopted.
        let supplied = get("client_id").filter(|c| !c.is_empty());
        let resolved = match (&client_id, supplied) {
            (None, Some(id)) if id != DYNAMIC_CLIENT_ID => id,
            (Some(saved), None) => saved.clone(),
            (Some(saved), Some(id)) if id == *saved => saved.clone(),
            (Some(_), Some(_)) => {
                respond(
                    &mut stream,
                    "400 Bad Request",
                    "Notchbud",
                    "This sign-in does not match the selected account.",
                );
                return Err(AuthError::new(
                    "Unexpected sign-in reply. Try again.",
                    true,
                ));
            }
            (None, _) => {
                respond(
                    &mut stream,
                    "400 Bad Request",
                    "Notchbud",
                    "Registration did not complete. Start again from the app.",
                );
                return Err(AuthError::new(
                    "Registration incomplete. Start again.",
                    false,
                ));
            }
        };
        respond(
            &mut stream,
            "200 OK",
            "Notchbud is connected",
            "You approved ChatGPT access. You can close this page and return to the app.",
        );
        return Ok((
            Callback {
                code,
                client_id: resolved,
            },
            PendingAuthMeta {
                redirect_uri,
                nonce,
                verifier,
                client_id,
            },
        ));
    }
}

pub struct PendingAuthMeta {
    pub redirect_uri: String,
    pub nonce: String,
    pub verifier: String,
    pub client_id: Option<String>,
}

// ---------------------------------------------------------------------------
// HTTP helpers
// ---------------------------------------------------------------------------

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
            "Offline. Check connection and try again.",
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
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", k, percent_encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

fn post_form(url: &str, body: &str) -> Result<ureq::Response, AuthError> {
    flatten(
        agent_http_ok()
            .post(url)
            .set("content-type", "application/x-www-form-urlencoded")
            .set("accept", "application/json")
            .send_string(body),
    )
}

fn body_json(resp: ureq::Response) -> Value {
    resp.into_string()
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .unwrap_or(Value::Null)
}

// ---------------------------------------------------------------------------
// token exchange + refresh
// ---------------------------------------------------------------------------

fn oauth_error_code(data: &Value) -> Option<String> {
    // Bodies vary: {"error":"invalid_grant"} or {"error":{"code":"..."}}.
    match &data["error"] {
        Value::String(s) => Some(s.clone()),
        Value::Object(_) => data["error"]["code"]
            .as_str()
            .or_else(|| data["error"]["message"].as_str())
            .map(str::to_owned),
        _ => None,
    }
}

/// Exchange the callback code. Uses the issued client id (never the dynamic
/// entrypoint), the same redirect URI, verifier, and resource. If the code is
/// stale, the caller discards it and starts a fresh authorization.
pub fn exchange_code(
    meta: &PendingAuthMeta,
    code: &str,
    client_id: &str,
) -> Result<Value, AuthError> {
    let resp = post_form(
        TOKEN_URL,
        &form(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("code_verifier", &meta.verifier),
            ("redirect_uri", &meta.redirect_uri),
            ("client_id", client_id),
            ("resource", RESOURCE),
        ]),
    )?;
    let status = resp.status();
    let data = body_json(resp);
    if status != 200 {
        if oauth_error_code(&data).as_deref() == Some("invalid_grant") {
            return Err(AuthError::new(
                "Approval expired. Connect again.",
                false,
            ));
        }
        return Err(AuthError::new(
            "Sign-in failed. Try again.",
            true,
        ));
    }
    Ok(data)
}

// ---------------------------------------------------------------------------
// ID-token validation (JWKS, RS256)
// ---------------------------------------------------------------------------

pub struct IdClaims {
    pub subject: String,
    pub email: Option<String>,
    pub name: Option<String>,
}

/// Display-name width for the island greeting line (same 420u budget the
/// manual name box used to enforce).
const NAME_MAX_CHARS: usize = 30;

fn non_empty_str(v: &Value) -> Option<&str> {
    v.as_str().filter(|s| !s.trim().is_empty())
}

/// Sanitize an OpenAI account display name for the island: squeeze
/// whitespace, reject control characters, truncate to the greeting width.
/// Returns None when nothing displayable remains.
pub fn sanitize_account_name(raw: &str) -> Option<String> {
    if raw.chars().any(|c| c.is_control()) {
        return None;
    }
    let squeezed = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if squeezed.is_empty() {
        return None;
    }
    let truncated: String = squeezed.chars().take(NAME_MAX_CHARS).collect();
    let truncated = truncated.trim_end().to_owned();
    if truncated.is_empty() {
        None
    } else {
        Some(truncated)
    }
}

/// Pick the display name out of validated ID-token claims: `name` first,
/// then given+family, then the OpenID username fallbacks.
pub fn account_name_from_claims(claims: &Value) -> Option<String> {
    if let Some(n) = non_empty_str(&claims["name"]) {
        if let Some(s) = sanitize_account_name(n) {
            return Some(s);
        }
    }
    let given = non_empty_str(&claims["given_name"]);
    let family = non_empty_str(&claims["family_name"]);
    match (given, family) {
        (Some(g), Some(f)) => {
            let joined = format!("{g} {f}");
            if let Some(s) = sanitize_account_name(&joined) {
                return Some(s);
            }
        }
        (Some(g), None) => {
            if let Some(s) = sanitize_account_name(g) {
                return Some(s);
            }
        }
        (None, Some(f)) => {
            if let Some(s) = sanitize_account_name(f) {
                return Some(s);
            }
        }
        (None, None) => {}
    }
    for key in ["preferred_username", "nickname"] {
        if let Some(n) = non_empty_str(&claims[key]) {
            // Usernames often arrive as an email; greet with the local part.
            let handle = n.split('@').next().unwrap_or(n);
            // Underscores/dots read as spaces in a greeting ("alex_kumar").
            let spaced = handle.replace(['_', '.', '-'], " ");
            if let Some(s) = sanitize_account_name(&spaced) {
                return Some(s);
            }
        }
    }
    None
}

fn fetch_jwks() -> Result<Value, AuthError> {
    let resp = flatten(
        agent_http_ok()
            .get(JWKS_URL)
            .set("accept", "application/json")
            .call(),
    )?;
    if resp.status() != 200 {
        return Err(AuthError::new(
            "Offline. Check connection and try again.",
            false,
        ));
    }
    Ok(body_json(resp))
}

fn verify_rs256(jwks: &Value, kid: &str, signing_input: &str, sig: &[u8]) -> Result<(), AuthError> {
    use rsa::{pkcs1v15::VerifyingKey, signature::Verifier, BigUint, RsaPublicKey};
    use sha2::Sha256;

    let keys = jwks
        .get("keys")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let key = keys
        .iter()
        .find(|k| k["kty"] == "RSA" && k["kid"] == kid)
        .ok_or_else(|| {
            AuthError::new("Verification failed. Start again.", false)
        })?;
    let n = key["n"].as_str().and_then(b64url).ok_or_else(|| {
        AuthError::new("Verification failed. Start again.", false)
    })?;
    let e = key["e"].as_str().and_then(b64url).ok_or_else(|| {
        AuthError::new("Verification failed. Start again.", false)
    })?;
    let pubkey = RsaPublicKey::new(BigUint::from_bytes_be(&n), BigUint::from_bytes_be(&e))
        .map_err(|_| {
            AuthError::new("Verification failed. Start again.", false)
        })?;
    let verifying = VerifyingKey::<Sha256>::new(pubkey);
    let signature = rsa::pkcs1v15::Signature::try_from(sig).map_err(|_| {
        AuthError::new("Verification failed. Start again.", false)
    })?;
    verifying
        .verify(signing_input.as_bytes(), &signature)
        .map_err(|_| AuthError::new("Verification failed. Start again.", false))
}

/// Verify the ID token's signature against OpenAI's JWKS and check issuer,
/// audience (the issued client id), expiry, and the saved nonce. Returns the
/// validated identity; the `sub` is the stable account identity.
pub fn validate_id_token(
    id_token: &str,
    expected_client_id: &str,
    expected_nonce: &str,
) -> Result<IdClaims, AuthError> {
    let bad = || AuthError::new("Verification failed. Start again.", false);
    let mut parts = id_token.split('.');
    let (header_b64, payload_b64, sig_b64) =
        match (parts.next(), parts.next(), parts.next(), parts.next()) {
            (Some(h), Some(p), Some(s), None) => (h, p, s),
            _ => return Err(bad()),
        };
    let header: Value =
        serde_json::from_slice(&b64url(header_b64).ok_or_else(bad)?).map_err(|_| bad())?;
    if header["alg"] != "RS256" {
        return Err(bad());
    }
    let kid = header["kid"].as_str().ok_or_else(bad)?.to_owned();
    let sig = b64url(sig_b64).ok_or_else(bad)?;
    let signing_input = format!("{header_b64}.{payload_b64}");

    // Fetch fresh keys when the kid is unfamiliar (rotation).
    let mut jwks = fetch_jwks()?;
    if !jwks
        .get("keys")
        .and_then(Value::as_array)
        .map(|keys| keys.iter().any(|k| k["kty"] == "RSA" && k["kid"] == kid))
        .unwrap_or(false)
    {
        jwks = fetch_jwks()?;
    }
    verify_rs256(&jwks, &kid, &signing_input, &sig)?;

    let claims: Value =
        serde_json::from_slice(&b64url(payload_b64).ok_or_else(bad)?).map_err(|_| bad())?;
    if claims["iss"] != ISSUER {
        return Err(bad());
    }
    let aud_ok = match &claims["aud"] {
        Value::String(a) => a == expected_client_id,
        Value::Array(list) => list.iter().any(|a| a == expected_client_id),
        _ => false,
    };
    if !aud_ok {
        return Err(bad());
    }
    let exp = claims["exp"]
        .as_u64()
        .or_else(|| claims["exp"].as_f64().map(|e| e as u64));
    match exp {
        Some(e) if e + CLOCK_SKEW_SECS > now_secs() => {}
        _ => return Err(bad()),
    }
    if claims["nonce"] != expected_nonce {
        return Err(bad());
    }
    let subject = claims["sub"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(bad)?
        .to_owned();
    let email = claims["email"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let name = account_name_from_claims(&claims);
    Ok(IdClaims {
        subject,
        email,
        name,
    })
}

// ---------------------------------------------------------------------------
// from token response to saved credentials
// ---------------------------------------------------------------------------

fn scopes_of(data: &Value) -> Vec<String> {
    data["scope"]
        .as_str()
        .map(|s| s.split_whitespace().map(str::to_owned).collect())
        .unwrap_or_default()
}

/// Build the credential set from a token response. For a returning account,
/// confirm the new ID token names the same identity before replacing anything.
pub fn finish_signin(
    data: &Value,
    client_id: &str,
    nonce: &str,
    host_id: &str,
    prev: Option<&Tokens>,
) -> Result<Tokens, AuthError> {
    let bad = || {
        AuthError::new(
            "Unexpected sign-in reply. Try again.",
            false,
        )
    };
    let access = data["access_token"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(bad)?
        .to_owned();
    let id_token = data["id_token"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(bad)?
        .to_owned();
    // A code exchange must return a refresh token (offline_access); a refresh
    // response may omit it, keeping the previous one.
    let refresh = match data.get("refresh_token") {
        Some(Value::String(s)) if !s.is_empty() => s.clone(),
        _ => prev
            .map(|t| t.refresh.clone())
            .filter(|s| !s.is_empty())
            .ok_or_else(bad)?,
    };
    let claims = validate_id_token(&id_token, client_id, nonce)?;
    if let Some(p) = prev {
        if p.subject != claims.subject {
            return Err(AuthError::new(
                "Wrong account. Start again for the selected one.",
                true,
            ));
        }
    }
    let expires_at = now_secs() + positive_secs(data.get("expires_in"), 3600);
    let scopes = scopes_of(data);
    let plan_allowed = scopes.iter().any(|s| s == PLAN_SCOPE);
    Ok(Tokens {
        access,
        refresh,
        id_token,
        expires_at,
        client_id: client_id.to_owned(),
        subject: claims.subject,
        email: claims.email,
        name: claims.name,
        scopes,
        plan_allowed,
        host_id: host_id.to_owned(),
    })
}

/// A new access token. OpenAI rotates the refresh token on every use, so only
/// one caller refreshes at a time and must save the answer before anyone else
/// refreshes: a second refresh with the old token fails.
pub fn refresh_tokens(prev: &Tokens) -> Result<Tokens, AuthError> {
    let resp = post_form(
        TOKEN_URL,
        &form(&[
            ("grant_type", "refresh_token"),
            ("client_id", &prev.client_id),
            ("refresh_token", &prev.refresh),
            ("resource", RESOURCE),
        ]),
    )?;
    if resp.status() != 200 {
        let status = resp.status();
        let data = body_json(resp);
        let code = oauth_error_code(&data).unwrap_or_default();
        // Unusable refresh token: only a new sign-in fixes it.
        let unusable = matches!(
            code.as_str(),
            "invalid_grant"
                | "invalid_refresh_token"
                | "token_expired"
                | "refresh_token_expired"
                | "refresh_token_invalidated"
                | "refresh_token_reused"
        ) || status == 400
            || status == 401;
        if code == "invalid_client" {
            return Err(AuthError::new(
                "Sign-in config invalid. Connect again from the tray.",
                true,
            ));
        }
        if unusable {
            return Err(AuthError::new(
                "Sign-in expired. Connect again from the tray.",
                true,
            ));
        }
        return Err(AuthError::new(
            "Couldn't refresh sign-in. Try again.",
            false,
        ));
    }
    let data = body_json(resp);
    let access = data["access_token"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            AuthError::new(
                "Unexpected sign-in reply. Try again.",
                false,
            )
        })?
        .to_owned();
    // The refresh response rotates the token; keep the previous one only if
    // the response omits the replacement.
    let refresh = match data.get("refresh_token") {
        Some(Value::String(s)) if !s.is_empty() => s.clone(),
        _ => prev.refresh.clone(),
    };
    // A refresh may or may not re-issue an ID token; the retained one keeps
    // working as a future id_token_hint either way.
    let id_token = match data.get("id_token") {
        Some(Value::String(s)) if !s.is_empty() => s.clone(),
        _ => prev.id_token.clone(),
    };
    let scopes = if data.get("scope").and_then(Value::as_str).is_some() {
        scopes_of(&data)
    } else {
        prev.scopes.clone()
    };
    Ok(Tokens {
        access,
        refresh,
        id_token,
        expires_at: now_secs() + positive_secs(data.get("expires_in"), 3600),
        client_id: prev.client_id.clone(),
        subject: prev.subject.clone(),
        email: prev.email.clone(),
        name: prev.name.clone(),
        scopes: scopes.clone(),
        plan_allowed: scopes.iter().any(|s| s == PLAN_SCOPE),
        host_id: prev.host_id.clone(),
    })
}

pub fn needs_refresh(tokens: &Tokens) -> bool {
    tokens.expires_at.saturating_sub(now_secs()) <= REFRESH_MARGIN_SECS
}

/// Attempt remote revocation of the renewable session (sign-out). An empty
/// HTTP 200 is success, including for an already-invalid token. Best effort:
/// the caller always clears local credentials afterwards.
pub fn revoke_session(tokens: &Tokens) {
    let body = form(&[
        ("token", &tokens.refresh),
        ("token_type_hint", "refresh_token"),
        ("client_id", &tokens.client_id),
    ]);
    let _ = post_form(REVOCATION_URL, &body);
}

// ---------------------------------------------------------------------------
// model catalog (per-account, no hardcoded fallback)
// ---------------------------------------------------------------------------

/// One displayable model: `display_name` for the UI, `slug` for inference.
#[derive(Clone)]
pub struct ModelInfo {
    pub slug: String,
    pub display_name: String,
}

/// The account's model catalog in the server's order (`visibility == "list"`).
/// Errors are the caller's fallback path.
pub fn list_models(tokens: &Tokens) -> Result<Vec<ModelInfo>, String> {
    let agent = ureq::AgentBuilder::new()
        .timeout(AUTH_TIMEOUT)
        .user_agent(USER_AGENT)
        .build();
    let resp = match agent
        .get(MODELS_URL)
        .set("accept", "application/json")
        .set("authorization", &format!("Bearer {}", tokens.access))
        .call()
    {
        Ok(r) => r,
        Err(ureq::Error::Status(_, r)) => r,
        Err(_) => return Err("offline — can't reach ChatGPT".to_owned()),
    };
    if resp.status() == 401 {
        return Err("expired".to_owned());
    }
    if resp.status() != 200 {
        return Err(format!(
            "ChatGPT models unavailable (HTTP {}).",
            resp.status()
        ));
    }
    let data = body_json(resp);
    // Documented shape is a `models` array; accept `data` (id/name) too.
    let items: &[Value] = data["models"]
        .as_array()
        .or_else(|| data["data"].as_array())
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let mut out = Vec::new();
    for item in items {
        let slug = item["slug"]
            .as_str()
            .or_else(|| item["id"].as_str())
            .unwrap_or("");
        if slug.is_empty() {
            continue;
        }
        if item.get("visibility").and_then(Value::as_str) == Some("hidden") {
            continue;
        }
        if let Some(v) = item.get("visibility").and_then(Value::as_str) {
            if v != "list" && item.get("slug").is_some() {
                continue;
            }
        }
        if !slug
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | ':' | '_' | '-'))
            || slug.len() > 128
        {
            continue;
        }
        let display = item["display_name"]
            .as_str()
            .or_else(|| item["name"].as_str())
            .unwrap_or(slug)
            .to_owned();
        out.push(ModelInfo {
            slug: slug.to_owned(),
            display_name: display,
        });
    }
    Ok(out)
}

/// The first catalog entry. No hardcoded fallback: an empty catalog means the
/// account has no models to offer, which the caller reports as-is.
pub fn pick_model(tokens: &Tokens) -> Result<String, String> {
    match list_models(tokens) {
        Ok(list) => list
            .into_iter()
            .next()
            .map(|m| m.slug)
            .ok_or_else(|| "No models are available on your ChatGPT plan right now.".to_owned()),
        Err(e) if e == "expired" => Err("expired".to_owned()),
        Err(e) => Err(e),
    }
}

// ---------------------------------------------------------------------------
// token store: DPAPI blob at %APPDATA%\notchbud\auth.bin — never plaintext
// ---------------------------------------------------------------------------

fn store_path() -> Option<std::path::PathBuf> {
    std::env::var_os("APPDATA").map(|base| {
        std::path::Path::new(&base)
            .join("notchbud")
            .join("auth.bin")
    })
}

unsafe fn dpapi(data: &[u8], protect: bool) -> Result<Vec<u8>, String> {
    let mut out = CRYPT_INTEGER_BLOB::default();
    let input = CRYPT_INTEGER_BLOB {
        cbData: data.len() as u32,
        pbData: data.as_ptr() as *mut u8,
    };
    let res = if protect {
        CryptProtectData(
            &input,
            w!("notchbud"),
            None,
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut out,
        )
    } else {
        CryptUnprotectData(
            &input,
            None,
            None,
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut out,
        )
    };
    if res.is_err() {
        return Err("dpapi".into());
    }
    let blob = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
    let _ = LocalFree(Some(windows::Win32::Foundation::HLOCAL(out.pbData as *mut _)));
    Ok(blob)
}

pub fn save_tokens(tokens: &Tokens) -> Result<(), String> {
    let plain = serde_json::json!({
        "v": 3,
        "accessToken": tokens.access,
        "refreshToken": tokens.refresh,
        "idToken": tokens.id_token,
        "expiresAt": tokens.expires_at,
        "clientId": tokens.client_id,
        "subject": tokens.subject,
        "email": tokens.email,
        "name": tokens.name,
        "scopes": tokens.scopes,
        "planAllowed": tokens.plan_allowed,
        "hostId": tokens.host_id,
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
    let path = store_path()?;
    let blob = std::fs::read(&path).ok()?;
    let plain = unsafe { dpapi(&blob, false) }.ok()?;
    let value = serde_json::from_slice::<Value>(&plain).ok()?;
    // v1 (Codex device-flow) and v2 (pre-name) blobs cannot be used as-is:
    // v1 cannot use the SIWC endpoints at all, and v2 has no account name
    // for the greeting — drop either so the user signs in again through
    // the browser flow and the name is captured.
    if value.get("v")?.as_i64()? != 3 {
        let _ = std::fs::remove_file(&path);
        return None;
    }
    Some(Tokens {
        access: value.get("accessToken")?.as_str()?.to_owned(),
        refresh: value.get("refreshToken")?.as_str()?.to_owned(),
        id_token: value.get("idToken")?.as_str()?.to_owned(),
        expires_at: value.get("expiresAt")?.as_u64()?,
        client_id: value.get("clientId")?.as_str()?.to_owned(),
        subject: value.get("subject")?.as_str()?.to_owned(),
        email: value
            .get("email")
            .and_then(Value::as_str)
            .map(str::to_owned),
        name: value
            .get("name")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_owned),
        scopes: value
            .get("scopes")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default(),
        plan_allowed: value.get("planAllowed")?.as_bool()?,
        host_id: value.get("hostId")?.as_str()?.to_owned(),
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
    fn b64url_roundtrip() {
        // known vector: {"exp":1}
        assert_eq!(b64url("eyJleHAiOjF9").unwrap(), b"{\"exp\":1}");
        for bytes in [&b"hello"[..], &b"\x00\xff\x10\x80"[..], &b""[..]] {
            let enc = b64url_encode(bytes);
            assert!(!enc.contains('='));
            assert_eq!(b64url(&enc).unwrap(), bytes);
        }
    }

    #[test]
    fn pkce_challenge_matches_rfc7636_vector() {
        // RFC 7636 Appendix B.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(
            pkce_challenge(verifier),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn percent_encode_escapes_url_characters() {
        assert_eq!(percent_encode("abc-_~.0"), "abc-_~.0");
        assert_eq!(percent_encode("a b&c+d/e"), "a%20b%26c%2Bd%2Fe");
    }

    #[test]
    fn percent_decode_roundtrip() {
        assert_eq!(percent_decode("a%20b%26c%2Bd%2Fe"), "a b&c+d/e");
        assert_eq!(percent_decode("plain"), "plain");
    }

    #[test]
    fn uuid_format_and_validation() {
        let r: Vec<u8> = (0..16).collect();
        assert_eq!(
            format_uuid(&r),
            "urn:uuid:00010203-0405-0607-0809-0a0b0c0d0e0f"
        );
        assert!(valid_host_id(
            "urn:uuid:123e4567-e89b-42d3-a456-426614174000"
        ));
        assert!(!valid_host_id("dynamic_agent_client"));
        assert!(!valid_host_id(""));
    }

    #[test]
    fn scopes_gate_plan_usage() {
        let data = serde_json::json!({"scope": "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct"});
        let scopes = scopes_of(&data);
        assert!(scopes.iter().any(|s| s == PLAN_SCOPE));
        let data = serde_json::json!({"scope": "openid profile email"});
        let scopes = scopes_of(&data);
        assert!(!scopes.iter().any(|s| s == PLAN_SCOPE));
    }

    #[test]
    fn query_params_decode() {
        let p = query_params("code=abc%20123&state=xyz&client_id=oaiapp_1");
        assert_eq!(p.len(), 3);
        assert_eq!(p[0], ("code".to_owned(), "abc 123".to_owned()));
    }

    #[test]
    fn account_name_prefers_name_claim() {
        let c = serde_json::json!({"name": "  Alex   Kumar  ", "given_name": "Alex"});
        assert_eq!(account_name_from_claims(&c).as_deref(), Some("Alex Kumar"));
    }

    #[test]
    fn account_name_falls_back_to_given_family_and_handles() {
        let c = serde_json::json!({"given_name": "Alex", "family_name": "Kumar"});
        assert_eq!(account_name_from_claims(&c).as_deref(), Some("Alex Kumar"));
        let c = serde_json::json!({"preferred_username": "alex_kumar@example.com"});
        assert_eq!(account_name_from_claims(&c).as_deref(), Some("alex kumar"));
        let c = serde_json::json!({"nickname": "Alex"});
        assert_eq!(account_name_from_claims(&c).as_deref(), Some("Alex"));
        let c = serde_json::json!({});
        assert_eq!(account_name_from_claims(&c), None);
    }

    #[test]
    fn account_name_sanitizes_and_truncates() {
        assert_eq!(sanitize_account_name("   "), None);
        assert_eq!(sanitize_account_name("a\nb"), None);
        assert_eq!(sanitize_account_name("José-O'Brien Jr."), Some("José-O'Brien Jr.".into()));
        let long = "a".repeat(40);
        let got = sanitize_account_name(&long).unwrap();
        assert_eq!(got.len(), 30);
    }
}
