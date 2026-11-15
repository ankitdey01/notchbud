// chat.rs — the island chat: a chat-completions request to Groq's
// OpenAI-compatible API with the user's own API key, and the SSE stream
// decoded into StreamEvents. A turn counts only after the stream ends with
// `data: [DONE]`; errors surface as-is.

use std::io::BufRead;
use std::time::Duration;

use serde_json::{json, Value};
use ureq::Agent;

use super::auth::USER_AGENT;

pub const GROQ_CHAT_URL: &str = "https://api.groq.com/openai/v1/chat/completions";

/// Terminal auth rejection: the key is dead — the caller must drop the
/// credential and return to key entry (CHAT_SIGNED_OUT), not CHAT_ERROR.
pub const AUTH_REJECTED_MSG: &str = "That Groq API key was rejected — enter a new key.";
/// The selected model is blocked for this project/key: the caller may try
/// the next catalog model once. Never clears the credential.
pub const MODEL_UNAVAILABLE_MSG: &str = "This model is not available — trying another helps.";

/// Everything chat sends is text; keep instructions plain.
const INSTRUCTIONS: &str = "You are notchbud, the tiny assistant inside a notch island on the desktop. Answer in one or two short plain-text sentences — no markdown, no lists.";

#[derive(Clone, Copy, PartialEq)]
pub enum Role {
    User,
    Assistant,
}

impl Role {
    pub fn code(&self) -> u8 {
        match self {
            Role::User => 0,
            Role::Assistant => 1,
        }
    }
    pub fn from_code(c: u8) -> Option<Role> {
        match c {
            0 => Some(Role::User),
            1 => Some(Role::Assistant),
            _ => None,
        }
    }
}

pub struct Msg {
    pub role: Role,
    pub text: String,
}

#[derive(Debug, PartialEq)]
pub enum StreamEvent {
    Delta(String),
    Done,
    Error(String),
}

fn chat_agent() -> Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(10))
        .timeout_read(Duration::from_secs(90))
        .user_agent(USER_AGENT)
        .build()
}

/// The Groq chat body: system instructions + short history, streamed.
/// Nothing stored server-side; the whole context rides every request.
pub fn chat_body(model: &str, history: &[Msg]) -> Value {
    let mut messages = Vec::with_capacity(history.len() + 1);
    messages.push(json!({"role": "system", "content": INSTRUCTIONS}));
    for m in history {
        messages.push(json!({
            "role": match m.role { Role::User => "user", Role::Assistant => "assistant" },
            "content": m.text,
        }));
    }
    json!({
        "model": model,
        "messages": messages,
        "stream": true,
    })
}

/// One `data:` line of the OpenAI-compatible SSE stream.
fn decode_event(v: &Value) -> Option<StreamEvent> {
    if let Some(err) = v.get("error") {
        let msg = err["message"].as_str().filter(|s| !s.is_empty()).unwrap_or("Groq could not complete the request.");
        return Some(StreamEvent::Error(user_error(msg)));
    }
    let choice = v.get("choices")?.get(0)?;
    if let Some(delta) = choice["delta"]["content"].as_str() {
        if !delta.is_empty() {
            return Some(StreamEvent::Delta(delta.to_owned()));
        }
    }
    // Some servers put content under message for non-streaming shapes.
    if let Some(content) = choice["message"]["content"].as_str() {
        if !content.is_empty() {
            return Some(StreamEvent::Delta(content.to_owned()));
        }
    }
    match choice["finish_reason"].as_str() {
        // A finish reason alone never completes the turn: only `data:
        // [DONE]` does. Treating "stop"/"length" as Done masks a cut-off
        // stream (EOF without [DONE]) as success.
        Some("stop") | Some("length") => None,
        _ => None,
    }
}

/// Friendly lines for the slot: quota / invalid key / offline.
fn user_error(raw: &str) -> String {
    let low = raw.to_ascii_lowercase();
    if low.contains("invalid_api_key") || low.contains("invalid api key") || low.contains("incorrect api key") || low.contains("unauthorized") || low.contains("invalid_token") {
        AUTH_REJECTED_MSG.into()
    } else if low.contains("usage not included")
        || low.contains("usage_limit")
        || low.contains("rate limit")
        || low.contains("rate_limit")
        || low.contains("quota")
        || low.contains("limit reached")
        || low.contains("tokens per")
    {
        "Groq limit reached for now. Try again later.".into()
    } else if low.contains("model") && (low.contains("not") || low.contains("decommissioned") || low.contains("not found")) {
        MODEL_UNAVAILABLE_MSG.into()
    } else {
        // Truncate long provider text; ellipsis marks the cut.
        let truncated: String = raw.chars().take(140).collect();
        if raw.chars().count() > 140 {
            format!("{truncated}…")
        } else {
            truncated
        }
    }
}

/// Runs the request on the calling thread (a spawned std::thread in main.rs)
/// and feeds every event to `on`. Returning false from `on` stops reading and
/// cancels the stream quietly (island collapsed).
pub fn stream_chat(
    api_key: &str,
    model: &str,
    history: &[Msg],
    on: impl FnMut(StreamEvent) -> bool,
) -> Result<(), String> {
    let agent = chat_agent();
    let req = agent
        .post(GROQ_CHAT_URL)
        .set("authorization", &format!("Bearer {}", api_key.trim()))
        .set("accept", "text/event-stream")
        .set("content-type", "application/json");
    let resp = match req.send_string(&chat_body(model, history).to_string()) {
        Ok(r) => r,
        Err(ureq::Error::Status(_, r)) => r,
        Err(_) => return Err("offline — can't reach Groq".to_owned()),
    };
    if resp.status() == 401 {
        return Err(AUTH_REJECTED_MSG.to_owned());
    }
    // 403 on chat-completions is model/project-scoped (blocked model),
    // not a dead key: surface it as a retryable model error so the caller
    // can fall through to the next catalog model. (/models keeps 401/403
    // as "expired" — a catalog 403 really is a dead key.)
    if resp.status() == 403 {
        return Err(MODEL_UNAVAILABLE_MSG.to_owned());
    }
    if resp.status() != 200 {
        let status = resp.status();
        let snippet = resp.into_string().unwrap_or_default();
        let v: Value = serde_json::from_str(&snippet).unwrap_or(Value::Null);
        let raw = v["error"]["message"]
            .as_str()
            .filter(|s| !s.is_empty())
            .unwrap_or(&snippet);
        let msg = match status {
            404 => MODEL_UNAVAILABLE_MSG.to_owned(),
            429 => "Groq limit reached for now. Try again later.".to_owned(),
            503 => "Groq is unavailable right now. Try again later.".to_owned(),
            _ => user_error(if raw.is_empty() {
                "Groq could not complete the request."
            } else {
                raw
            }),
        };
        return Err(msg);
    }

    let reader = std::io::BufReader::new(resp.into_reader()); // into_reader is a plain Read
    pump_sse(reader, on)
}

/// Fold one SSE byte stream into events. Success only after the terminal
/// `data: [DONE]` marker: EOF after a finish-reason chunk (or anything
/// else) without [DONE] is a cut-off failure, not a partial answer.
/// Split out so tests can drive it without the network.
fn pump_sse<R: BufRead>(mut reader: R, mut on: impl FnMut(StreamEvent) -> bool) -> Result<(), String> {
    let mut line = String::new();
    let mut completed = false;
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break, // stream ended
            Ok(_) => {}
            Err(_) => return Err("The reply was cut off. Try again.".to_owned()),
        }
        let Some(rest) = line.strip_prefix("data:") else {
            continue;
        };
        let rest = rest.trim();
        if rest.is_empty() {
            continue;
        }
        if rest == "[DONE]" {
            completed = true;
            let _ = on(StreamEvent::Done);
            break;
        }
        let Ok(v) = serde_json::from_str::<Value>(rest) else {
            continue;
        };
        match decode_event(&v) {
            None => {}
            Some(StreamEvent::Delta(d)) => {
                if !d.is_empty() && !on(StreamEvent::Delta(d)) {
                    return Ok(()); // cancelled by the UI
                }
            }
            Some(StreamEvent::Done) => {
                completed = true;
                let _ = on(StreamEvent::Done);
                break;
            }
            Some(StreamEvent::Error(m)) => return Err(m),
        }
    }
    // Success only after the terminal marker: an interrupted stream is a
    // failure, not a partial answer.
    if completed {
        Ok(())
    } else {
        Err("The reply was cut off. Try again.".to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_events_decode() {
        assert_eq!(
            decode_event(
                &serde_json::from_str::<Value>(
                    r#"{"choices":[{"delta":{"content":"Hi"},"finish_reason":null}]}"#
                )
                .unwrap()
            ),
            Some(StreamEvent::Delta("Hi".into()))
        );
        assert_eq!(
            decode_event(
                &serde_json::from_str::<Value>(
                    r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#
                )
                .unwrap()
            ),
            // A finish reason is not completion: only `data: [DONE]` ends
            // the turn successfully (EOF without [DONE] is cut off).
            None
        );
        assert_eq!(
            decode_event(&serde_json::from_str::<Value>(
                r#"{"error":{"message":"Invalid API Key"}}"#
            ).unwrap()),
            Some(StreamEvent::Error(
                "That Groq API key was rejected — enter a new key.".into()
            ))
        );
        assert_eq!(
            decode_event(
                &serde_json::from_str::<Value>(r#"{"choices":[{"delta":{},"finish_reason":null}]}"#)
                    .unwrap()
            ),
            None
        );
    }

    #[test]
    fn chat_body_shape() {
        let body = chat_body(
            "openai/gpt-oss-20b",
            &[
                Msg {
                    role: Role::User,
                    text: "hi".into(),
                },
                Msg {
                    role: Role::Assistant,
                    text: "hey".into(),
                },
            ],
        );
        assert_eq!(body["model"], "openai/gpt-oss-20b");
        assert_eq!(body["stream"], true);
        assert_eq!(body["messages"][0]["role"], "system");
        assert!(body["messages"][0]["content"].as_str().unwrap().contains("notchbud"));
        assert_eq!(body["messages"][1]["role"], "user");
        assert_eq!(body["messages"][1]["content"], "hi");
        assert_eq!(body["messages"][2]["role"], "assistant");
        assert_eq!(body["messages"][2]["content"], "hey");
    }

    #[test]
    fn user_error_maps_key_and_quota() {
        assert!(user_error("Invalid API Key").contains("rejected"));
        assert!(user_error("Rate limit reached for tokens per minute").contains("limit"));
    }

    #[test]
    fn stream_fails_on_eof_without_done_marker() {
        // A finish-reason chunk with no trailing `data: [DONE]` is a
        // cut-off stream, not a completed turn.
        let body = "data: {\"choices\":[{\"delta\":{\"content\":\"Hi\"},\"finish_reason\":null}]}\n\
                    data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n";
        let mut deltas = Vec::new();
        let r = pump_sse(std::io::Cursor::new(body), |ev| match ev {
            StreamEvent::Delta(d) => {
                deltas.push(d);
                true
            }
            _ => true,
        });
        assert_eq!(r, Err("The reply was cut off. Try again.".to_owned()));
        assert_eq!(deltas, vec!["Hi".to_owned()]);
    }

    #[test]
    fn stream_completes_on_done_marker() {
        let body = "data: {\"choices\":[{\"delta\":{\"content\":\"Hi\"},\"finish_reason\":null}]}\n\
                    data: [DONE]\n";
        let mut saw_done = false;
        let r = pump_sse(std::io::Cursor::new(body), |ev| {
            if ev == StreamEvent::Done {
                saw_done = true;
            }
            true
        });
        assert_eq!(r, Ok(()));
        assert!(saw_done);
    }
}
