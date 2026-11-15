// auth.rs — Groq API-key auth: one key, sealed with DPAPI.
//
// No OAuth, no browser, no JWKS, no refresh: the island prompts for a Groq
// API key once (`https://console.groq.com/keys`, `gsk_...`), it is stored as
// a DPAPI blob at %APPDATA%\notchbud\auth.bin (never plaintext), and every
// chat/model call sends it as `Authorization: Bearer <key>` to
// `https://api.groq.com/openai/v1/*` (OpenAI-compatible path).

use serde_json::Value;
use ureq::Agent;

use windows::core::w;
use windows::Win32::Foundation::{HLOCAL, LocalFree};
use windows::Win32::Security::Cryptography::{
    CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
};

pub const GROQ_MODELS_URL: &str = "https://api.groq.com/openai/v1/models";
pub const USER_AGENT: &str = "notchbud/0.1";
const AUTH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(12);

/// What a sign-in saves. Never logged; it is a credential.
#[derive(Clone)]
pub struct ApiKey {
    pub key: String,
}

impl ApiKey {
    pub fn new(key: &str) -> Option<Self> {
        let key = key.trim().to_owned();
        if is_valid_key_format(&key) {
            Some(ApiKey { key })
        } else {
            None
        }
    }
}

/// Groq keys are `gsk_...` (48+ chars in practice). Gate typos locally so a
/// pasted URL or empty string never reaches the network.
pub fn is_valid_key_format(key: &str) -> bool {
    let key = key.trim();
    key.len() >= 20 && key.starts_with("gsk_") && !key.chars().any(|c| c.is_whitespace() || c.is_control())
}

/// A sign-in failure with a message we wrote, safe to show as is. `expired`
/// means the saved key is rejected; anything else may succeed on retry.
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

fn agent_http_ok() -> Agent {
    ureq::AgentBuilder::new()
        .timeout(AUTH_TIMEOUT)
        .user_agent(USER_AGENT)
        .build()
}

fn body_json(resp: ureq::Response) -> Value {
    resp.into_string()
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .unwrap_or(Value::Null)
}

// ---------------------------------------------------------------------------
// model catalog (per key, no hardcoded fallback)
// ---------------------------------------------------------------------------

/// One displayable model: `display_name` for the UI, `slug` for inference.
#[derive(Clone)]
pub struct ModelInfo {
    pub slug: String,
    pub display_name: String,
}

/// The account's model catalog in the server's order. Errors are the caller's
/// fallback path; an invalid key surfaces as `"expired"`.
pub fn list_models(api_key: &str) -> Result<Vec<ModelInfo>, String> {
    let agent = agent_http_ok();
    let resp = match agent
        .get(GROQ_MODELS_URL)
        .set("accept", "application/json")
        .set("authorization", &format!("Bearer {}", api_key.trim()))
        .call()
    {
        Ok(r) => r,
        Err(ureq::Error::Status(_, r)) => r,
        Err(_) => return Err("offline — can't reach Groq".to_owned()),
    };
    if resp.status() == 401 || resp.status() == 403 {
        return Err("expired".to_owned());
    }
    if resp.status() != 200 {
        return Err(format!("Groq models unavailable (HTTP {}).", resp.status()));
    }
    let data = body_json(resp);
    // Groq (OpenAI-compatible) shape is a `data` array of {id,...}.
    let items: &[Value] = data["data"].as_array().map(Vec::as_slice).unwrap_or(&[]);
    let mut out = Vec::new();
    for item in items {
        let slug = item["id"].as_str().unwrap_or("");
        if slug.is_empty() {
            continue;
        }
        if item.get("active").and_then(Value::as_bool) == Some(false) {
            continue;
        }
        if !slug
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | ':' | '_' | '-' | '/'))
            || slug.len() > 128
        {
            continue;
        }
        out.push(ModelInfo {
            slug: slug.to_owned(),
            // Groq has no separate display name; the id is the name.
            display_name: slug.to_owned(),
        });
    }
    Ok(out)
}

/// Chat models Groq serves on the developer plan right now
/// (https://console.groq.com/docs/models, production text models).
/// Fastest/cheapest first: the island answers in one or two sentences, so
/// latency beats size. `llama-3.1-8b-instant` / `llama-3.3-70b-versatile`
/// are enterprise-only since Aug 2026 and must NOT be preferred.
pub const PREFERRED_CHAT_MODELS: [&str; 2] =
    ["openai/gpt-oss-20b", "openai/gpt-oss-120b"];

/// True when the slug can back a chat-completions turn. Excludes speech,
/// safety and TTS entries that share the same catalog but cannot chat
/// (whisper, prompt-guard, safeguard, orpheus). Matched case-insensitively.
fn is_chat_model(slug: &str) -> bool {
    let low = slug.to_ascii_lowercase();
    !["whisper", "guard", "safeguard", "orpheus", "tts", "transcri", "embed"]
        .iter()
        .any(|b| low.contains(b))
}

/// Pure pick over catalog slugs in server order, skipping any slug in
/// `excluded` (e.g. a model the project just rejected with 403) so a
/// blocked model is not repeatedly selected. The preferred production
/// chat model wins first, else the first remaining chat-capable entry.
/// `None` = nothing to chat with. No network — unit-testable.
pub fn pick_model_from_excluding<'a>(
    slugs: impl IntoIterator<Item = &'a str>,
    excluded: &[&str],
) -> Option<String> {
    let slugs: Vec<&str> = slugs
        .into_iter()
        .filter(|s| !excluded.contains(s))
        .collect();
    for want in PREFERRED_CHAT_MODELS {
        if slugs.iter().any(|s| *s == want) {
            return Some(want.to_owned());
        }
    }
    slugs
        .into_iter()
        .find(|s| is_chat_model(s))
        .map(str::to_owned)
}

/// Pure pick over catalog slugs in server order: the preferred production
/// chat model first, else the first remaining chat-capable entry.
/// `None` = nothing to chat with. No network — unit-testable.
pub fn pick_model_from<'a>(slugs: impl IntoIterator<Item = &'a str>) -> Option<String> {
    pick_model_from_excluding(slugs, &[])
}

/// The account's first usable chat model, skipping `excluded` slugs. No
/// hardcoded fallback: an empty (or chat-less) catalog means the key has
/// no models to offer, which the caller reports as-is.
pub fn pick_model_excluding(api_key: &str, excluded: &[&str]) -> Result<String, String> {
    match list_models(api_key) {
        Ok(list) => pick_model_from_excluding(list.iter().map(|m| m.slug.as_str()), excluded)
            .ok_or_else(|| "No models are available for this Groq API key right now.".to_owned()),
        Err(e) => Err(e),
    }
}

/// The account's first usable chat model. No hardcoded fallback: an empty
/// (or chat-less) catalog means the key has no models to offer, which the
/// caller reports as-is.
pub fn pick_model(api_key: &str) -> Result<String, String> {
    pick_model_excluding(api_key, &[])
}

/// Save after a successful `list_models`: the key is real before it persists.
pub fn validate_api_key(api_key: &str) -> Result<(), AuthError> {
    match list_models(api_key) {
        Ok(_) => Ok(()),
        Err(e) if e == "expired" => Err(AuthError::new(
            "That Groq API key was rejected. Check it and try again.",
            true,
        )),
        Err(e) => Err(AuthError::new(e, false)),
    }
}

// ---------------------------------------------------------------------------
// key store: DPAPI blob at %APPDATA%\notchbud\auth.bin — never plaintext
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
    let _ = LocalFree(Some(HLOCAL(out.pbData as *mut _)));
    Ok(blob)
}

pub fn save_api_key(api_key: &ApiKey) -> Result<(), String> {
    let plain = serde_json::json!({
        "v": 4,
        "apiKey": api_key.key,
    })
    .to_string();
    let blob = unsafe { dpapi(plain.as_bytes(), true) }?;
    let path = store_path().ok_or("no %APPDATA%")?;
    std::fs::create_dir_all(path.parent().unwrap()).map_err(|_| "mkdir".to_owned())?;
    let tmp = path.with_extension("bin.tmp");
    std::fs::write(&tmp, &blob).map_err(|_| "write".to_owned())?;
    std::fs::rename(&tmp, &path).map_err(|_| "rename".to_owned())
}

pub fn load_api_key() -> Option<ApiKey> {
    let path = store_path()?;
    let blob = std::fs::read(&path).ok()?;
    let plain = unsafe { dpapi(&blob, false) }.ok()?;
    let value = serde_json::from_slice::<Value>(&plain).ok()?;
    // v1-v3 (ChatGPT OAuth) blobs cannot be used as-is: drop them so the
    // user enters a Groq key instead of silently failing validation.
    if value.get("v")?.as_i64()? != 4 {
        let _ = std::fs::remove_file(&path);
        return None;
    }
    let key = value.get("apiKey")?.as_str()?.trim().to_owned();
    ApiKey::new(&key)
}

pub fn delete_api_key() {
    if let Some(path) = store_path() {
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_format_gates_typos() {
        assert!(is_valid_key_format("gsk_1234567890abcdef"));
        assert!(ApiKey::new("  gsk_1234567890abcdef  ").is_some());
        assert!(ApiKey::new("").is_none());
        assert!(ApiKey::new("sk-abc").is_none());
        assert!(ApiKey::new("gsk_short").is_none());
        assert!(ApiKey::new("gsk_has space 1234567890").is_none());
        assert!(ApiKey::new("https://console.groq.com/keys").is_none());
    }

    #[test]
    fn api_key_trims_before_store_shape() {
        let k = ApiKey::new("  gsk_1234567890abcdef  ").unwrap();
        assert_eq!(k.key, "gsk_1234567890abcdef");
    }

    #[test]
    fn excluded_preferred_model_falls_through() {
        let catalog = ["openai/gpt-oss-20b", "openai/gpt-oss-120b"];
        assert_eq!(
            pick_model_from_excluding(catalog, &["openai/gpt-oss-20b"]),
            Some("openai/gpt-oss-120b".to_owned())
        );
        assert_eq!(
            pick_model_from_excluding(catalog, &["openai/gpt-oss-20b", "openai/gpt-oss-120b"]),
            None
        );
    }
}
