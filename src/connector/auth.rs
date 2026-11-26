// auth.rs — one active Groq or OpenAI-compatible connection, sealed with DPAPI.
// Saved keys never enter the WebView; custom keys are optional for local servers.

use serde_json::Value;
use ureq::Agent;

use windows::core::w;
use windows::Win32::Foundation::{LocalFree, HLOCAL};
use windows::Win32::Security::Cryptography::{
    CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
};

pub const GROQ_BASE_URL: &str = "https://api.groq.com/openai/v1";
pub const GROQ_MODELS_URL: &str = "https://api.groq.com/openai/v1/models";
pub const USER_AGENT: &str = concat!("notchbud/", env!("CARGO_PKG_VERSION"));
const AUTH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(12);

#[derive(Clone, Debug, PartialEq)]
pub enum Provider {
    Groq,
    Custom { base_url: String, model: String },
}

/// The complete active connection. Never logged; `key` is a credential.
#[derive(Clone, Debug, PartialEq)]
pub struct ApiKey {
    pub key: String,
    pub provider: Provider,
}

impl ApiKey {
    pub fn new(key: &str) -> Option<Self> {
        let key = key.trim().to_owned();
        if is_valid_key_format(&key) {
            Some(ApiKey {
                key,
                provider: Provider::Groq,
            })
        } else {
            None
        }
    }

    pub fn custom(base_url: &str, key: &str, model: &str) -> Result<Self, String> {
        let base_url = normalize_base_url(base_url)?;
        let key = key.trim();
        if key.len() > 512 || key.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err("Enter a valid API key, or leave it blank.".into());
        }
        let model = model.trim();
        if model.is_empty()
            || model.len() > 128
            || model.chars().any(|c| c.is_whitespace() || c.is_control())
        {
            return Err("Enter one valid model ID.".into());
        }
        Ok(ApiKey {
            key: key.to_owned(),
            provider: Provider::Custom {
                base_url,
                model: model.to_owned(),
            },
        })
    }

    pub fn is_custom(&self) -> bool {
        matches!(self.provider, Provider::Custom { .. })
    }

    pub fn fixed_model(&self) -> Option<&str> {
        match &self.provider {
            Provider::Groq => None,
            Provider::Custom { model, .. } => Some(model),
        }
    }

    pub fn custom_base_url(&self) -> Option<&str> {
        match &self.provider {
            Provider::Groq => None,
            Provider::Custom { base_url, .. } => Some(base_url),
        }
    }

    pub fn base_url(&self) -> &str {
        match &self.provider {
            Provider::Groq => GROQ_BASE_URL,
            Provider::Custom { base_url, .. } => base_url,
        }
    }

    pub fn models_url(&self) -> String {
        format!("{}/models", self.base_url())
    }

    pub fn chat_url(&self) -> String {
        format!("{}/chat/completions", self.base_url())
    }

    pub fn is_opencode_go(&self) -> bool {
        self.base_url() == "https://opencode.ai/inference/go/openai/v1"
    }
}

fn normalize_base_url(raw: &str) -> Result<String, String> {
    let raw = raw.trim();
    if raw.is_empty() || raw.len() > 2048 {
        return Err("Enter a valid base URL.".into());
    }
    let parsed = url::Url::parse(raw).map_err(|_| "Enter a valid base URL.".to_owned())?;
    let host = parsed.host().ok_or("The base URL needs a host.")?;
    let loopback = match host {
        url::Host::Domain(domain) => domain.eq_ignore_ascii_case("localhost"),
        url::Host::Ipv4(ip) => ip.is_loopback(),
        url::Host::Ipv6(ip) => ip.is_loopback(),
    };
    if (parsed.scheme() != "https" && !(parsed.scheme() == "http" && loopback))
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return Err(
            "Use HTTPS (or HTTP on localhost), without credentials, query, or fragment.".into(),
        );
    }
    let path = parsed.path().trim_end_matches('/');
    if path.ends_with("/chat/completions") || path.ends_with("/models") {
        return Err("Enter the API root, not a /models or /chat/completions URL.".into());
    }
    Ok(parsed.as_str().trim_end_matches('/').to_owned())
}

/// Groq keys are `gsk_...` (48+ chars in practice). Gate typos locally so a
/// pasted URL or empty string never reaches the network.
pub fn is_valid_key_format(key: &str) -> bool {
    let key = key.trim();
    key.len() >= 20
        && key.starts_with("gsk_")
        && !key.chars().any(|c| c.is_whitespace() || c.is_control())
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
        .redirects(0)
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

fn list_models_at(url: &str, api_key: &str, provider: &str) -> Result<Vec<ModelInfo>, String> {
    let agent = agent_http_ok();
    let mut req = agent.get(url).set("accept", "application/json");
    if !api_key.trim().is_empty() {
        req = req.set("authorization", &format!("Bearer {}", api_key.trim()));
    }
    let resp = match req.call() {
        Ok(r) => r,
        Err(ureq::Error::Status(_, r)) => r,
        Err(_) => return Err(format!("Couldn't reach {provider}. Try again.")),
    };
    if resp.status() == 401 || resp.status() == 403 {
        return Err("expired".to_owned());
    }
    if resp.status() != 200 {
        return Err(format!(
            "{provider} models unavailable (HTTP {}).",
            resp.status()
        ));
    }
    let data = body_json(resp);
    let Value::Array(items) = &data["data"] else {
        return Err(format!("{provider} models unavailable (bad response)."));
    };
    let mut out = Vec::new();
    for item in items {
        let slug = item["id"].as_str().unwrap_or("");
        if slug.is_empty() || item.get("active").and_then(Value::as_bool) == Some(false) {
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
            display_name: slug.to_owned(),
        });
    }
    Ok(out)
}

/// Groq's catalog retains its existing automatic model-selection behavior.
pub fn list_models(api_key: &str) -> Result<Vec<ModelInfo>, String> {
    list_models_at(GROQ_MODELS_URL, api_key, "Groq")
}

/// Check the custom endpoint before advancing from key entry. The catalog
/// also supplies suggestions for the editable model field.
pub fn discover_custom_models(base_url: &str, api_key: &str) -> Result<Vec<ModelInfo>, AuthError> {
    let draft = ApiKey::custom(base_url, api_key, "discovery").map_err(AuthError::from)?;
    match list_models_at(&draft.models_url(), &draft.key, "custom endpoint") {
        Ok(mut models) => {
            // Speech, safety, TTS and embedding entries share the catalog
            // but cannot back a chat turn — same rule as the Groq picker.
            models.retain(|m| is_chat_model(&m.slug));
            Ok(models)
        }
        Err(e) if e == "expired" => Err(AuthError::new(
            "The custom endpoint rejected this API key. Check it and try again.",
            true,
        )),
        Err(e) => Err(AuthError::new(e, false)),
    }
}

impl From<String> for AuthError {
    fn from(msg: String) -> Self {
        AuthError::new(msg, false)
    }
}

/// Chat models Groq serves on the developer plan right now
/// (https://console.groq.com/docs/models, production text models).
/// Fastest/cheapest first: the island answers in one or two sentences, so
/// latency beats size. `llama-3.1-8b-instant` / `llama-3.3-70b-versatile`
/// are enterprise-only since Aug 2026 and must NOT be preferred.
pub const PREFERRED_CHAT_MODELS: [&str; 2] = ["openai/gpt-oss-20b", "openai/gpt-oss-120b"];

/// Screenshot turns always run on this model: Groq's multimodal Qwen
/// (https://console.groq.com/docs/vision) takes image_url parts, which
/// the text-only preferred chat models reject. Fixed slug, not a catalog
/// pick — there is exactly one model for the job.
pub const SCREENSHOT_MODEL: &str = "qwen/qwen3.8-27b";

/// True when the slug can back a chat-completions turn. Excludes speech,
/// safety and TTS entries that share the same catalog but cannot chat
/// (whisper, prompt-guard, safeguard, orpheus). Matched case-insensitively.
fn is_chat_model(slug: &str) -> bool {
    let low = slug.to_ascii_lowercase();
    ![
        "whisper",
        "guard",
        "safeguard",
        "orpheus",
        "tts",
        "transcri",
        "embed",
    ]
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
/// A catalog with nothing to chat with fails here, not on first submit.
pub fn validate_api_key(api_key: &str) -> Result<(), AuthError> {
    match list_models(api_key) {
        Ok(list) => {
            if pick_model_from(list.iter().map(|m| m.slug.as_str())).is_none() {
                return Err(AuthError::new(
                    "No models are available for this Groq API key right now.",
                    false,
                ));
            }
            Ok(())
        }
        Err(e) if e == "expired" => Err(AuthError::new(super::chat::AUTH_REJECTED_MSG, true)),
        Err(e) => Err(AuthError::new(e, false)),
    }
}

// ---------------------------------------------------------------------------
// key store: DPAPI blob at %APPDATA%\notchbud\auth.bin — never plaintext
// ---------------------------------------------------------------------------

fn store_path() -> Option<std::path::PathBuf> {
    crate::data_dir().map(|dir| dir.join("auth.bin"))
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
    let mut value = serde_json::json!({
        "v": 5,
        "provider": if api_key.is_custom() { "custom" } else { "groq" },
        "apiKey": api_key.key,
    });
    if let Provider::Custom { base_url, model } = &api_key.provider {
        value["baseUrl"] = serde_json::json!(base_url);
        value["model"] = serde_json::json!(model);
    }
    let plain = value.to_string();
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
    // Older auth formats are deliberately not migrated; the user re-enters
    // the active provider instead of risking a half-migrated connection.
    if value.get("v")?.as_i64()? != 5 {
        let _ = std::fs::remove_file(&path);
        return None;
    }
    let key = value.get("apiKey")?.as_str()?.trim().to_owned();
    match value.get("provider")?.as_str()? {
        "groq" => ApiKey::new(&key),
        "custom" => ApiKey::custom(
            value.get("baseUrl")?.as_str()?,
            &key,
            value.get("model")?.as_str()?,
        )
        .ok(),
        _ => None,
    }
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
    fn chat_slug_filter_drops_non_chat_models() {
        // The custom catalog offers only this filtered set: plain chat
        // models pass, speech/safety/TTS/transcription/embeddings do not.
        for slug in [
            "kimi-k3",
            "openai/gpt-oss-20b",
            "qwen/qwen3.8-27b",
            "claude-haiku-4-5",
        ] {
            assert!(is_chat_model(slug), "{slug} should list");
        }
        for slug in [
            "whisper-large-v3-turbo",
            "canopylabs/orpheus-v1-english",
            "meta-llama/llama-prompt-guard-2-86m",
            "text-embedding-3-small",
            "nomic-embed-text:latest",
            "tts-1-hd",
            "gpt-4o-transcribe",
        ] {
            assert!(!is_chat_model(slug), "{slug} should not list");
        }
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
