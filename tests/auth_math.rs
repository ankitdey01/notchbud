// auth_math.rs — pure Groq key math: format gate, identity constants.
// No network, no DPAPI, no %APPDATA% writes: safe on any dev machine.
// (Deliberately NOT tested here: save/load/delete_api_key touch the real
// login at %APPDATA%\notchbud\auth.bin, and list/validate hit the
// network. Those stay manual.)

use notchbud::connector::auth::{is_valid_key_format, pick_model_from, ApiKey, USER_AGENT};
use notchbud::connector::chat::GROQ_CHAT_URL;

#[test]
fn key_format_gate() {
    assert!(is_valid_key_format("gsk_1234567890abcdef"));
    assert!(is_valid_key_format("  gsk_1234567890abcdef  "));
    assert!(!is_valid_key_format(""));
    assert!(!is_valid_key_format("sk-abc"));
    assert!(!is_valid_key_format("gsk_short"));
    assert!(!is_valid_key_format("gsk_has space 1234567890"));
    assert!(!is_valid_key_format("https://console.groq.com/keys"));
}

#[test]
fn api_key_trims() {
    let k = ApiKey::new("  gsk_1234567890abcdef  ").unwrap();
    assert_eq!(k.key, "gsk_1234567890abcdef");
    assert!(ApiKey::new("not-a-key").is_none());
}

#[test]
fn identity_consts() {
    assert_eq!(GROQ_CHAT_URL, "https://api.groq.com/openai/v1/chat/completions");
    assert!(
        USER_AGENT.starts_with("notchbud/"),
        "server-side originator tag rides on this"
    );
}

/// RED for the wrong-model bug: `pick_model` took the first catalog entry,
/// but Groq's server order is arbitrary — the first id can be whisper
/// (audio, can't chat), a guard model, or a deprecated llama. Per
/// https://console.groq.com/docs/models the production text models on a
/// developer key are openai/gpt-oss-20b and openai/gpt-oss-120b
/// (llama-3.1-8b-instant / llama-3.3-70b-versatile went enterprise-only and
/// deprecated for dev tier in Aug 2026).
#[test]
fn model_pick_prefers_fast_production_chat_model() {
    // Arrange: server-first junk, preferred model buried in the middle.
    let catalog = [
        "whisper-large-v3-turbo",
        "llama-3.3-70b-versatile",
        "openai/gpt-oss-120b",
        "openai/gpt-oss-20b",
        "qwen/qwen3.8-27b",
    ];
    // Act + assert: fastest production chat model wins, not server order.
    assert_eq!(
        pick_model_from(catalog),
        Some("openai/gpt-oss-20b".to_owned())
    );
}

#[test]
fn model_pick_falls_back_to_120b_without_20b() {
    let catalog = ["llama-3.3-70b-versatile", "openai/gpt-oss-120b"];
    assert_eq!(
        pick_model_from(catalog),
        Some("openai/gpt-oss-120b".to_owned())
    );
}

#[test]
fn model_pick_skips_non_chat_slugs() {
    // Arrange: no preferred model; only speech/safety/tts entries plus one
    // chat-capable preview model.
    let catalog = [
        "whisper-large-v3",
        "meta-llama/llama-prompt-guard-2-86m",
        "openai/gpt-oss-safeguard-20b",
        "canopylabs/orpheus-v1-english",
        "qwen/qwen3.8-27b",
    ];
    // Act + assert: first chat-capable entry, never whisper/guard/orpheus.
    assert_eq!(pick_model_from(catalog), Some("qwen/qwen3.8-27b".to_owned()));
}

#[test]
fn model_pick_is_none_without_any_chat_model() {
    assert_eq!(
        pick_model_from(["whisper-large-v3", "meta-llama/llama-prompt-guard-2-22m"]),
        None
    );
    let empty: [&str; 0] = [];
    assert_eq!(pick_model_from(empty), None);
}
