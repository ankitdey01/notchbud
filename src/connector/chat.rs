// chat.rs — the island chat: a Responses request to the Codex backend with
// the user's own ChatGPT tokens, and the SSE stream decoded into
// StreamEvents. Only output_text deltas + completed/failed are read; notch
// chat has no tool calls.

use std::io::BufRead;
use std::time::Duration;

use serde_json::{json, Value};
use ureq::Agent;

use super::auth::{Tokens, ORIGINATOR, USER_AGENT};

pub const RESPONSES_URL: &str = "https://chatgpt.com/backend-api/codex/responses";

/// Everything chat sends is text; keep instructions plain.
const INSTRUCTIONS: &str = "You are dyland, the tiny assistant inside a notch island on the desktop. Answer in one or two short plain-text sentences — no markdown, no lists.";

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

/// The Responses body for the island chat: the whole short history, no tools,
/// nothing stored.
pub fn responses_body(model: &str, history: &[Msg]) -> Value {
    let input: Vec<Value> = history
        .iter()
        .map(|m| {
            json!({
                "type": "message",
                "role": match m.role { Role::User => "user", Role::Assistant => "assistant" },
                "content": match m.role {
                    Role::User => json!([{ "type": "input_text", "text": m.text }]),
                    Role::Assistant => json!(m.text),
                },
            })
        })
        .collect();
    json!({
        "model": model,
        "instructions": INSTRUCTIONS,
        "input": input,
        "stream": true,
        "store": false,
        "reasoning": { "effort": "medium", "summary": "auto" },
    })
}

/// One `data:` line of the Responses event stream.
fn decode_event(v: &Value) -> Option<StreamEvent> {
    match v.get("type").and_then(Value::as_str) {
        Some("response.output_text.delta") => Some(StreamEvent::Delta(
            v.get("delta").and_then(Value::as_str).unwrap_or("").to_owned(),
        )),
        Some("response.completed") | Some("response.incomplete") | Some("response.done") => {
            Some(StreamEvent::Done)
        }
        Some("response.failed") => {
            let msg = v["response"]["error"]["message"]
                .as_str()
                .or_else(|| v["response"]["error"]["code"].as_str())
                .unwrap_or("ChatGPT could not complete the request.");
            Some(StreamEvent::Error(user_error(msg)))
        }
        Some("error") => {
            let msg = v["error"]["message"].as_str().unwrap_or("ChatGPT could not complete the request.");
            Some(StreamEvent::Error(user_error(msg)))
        }
        _ => None,
    }
}

/// Friendly lines for the slot: quota exhausted / sign-in expired / offline.
fn user_error(raw: &str) -> String {
    let low = raw.to_ascii_lowercase();
    if low.contains("usage not included") || low.contains("usage_limit") || low.contains("rate limit")
        || low.contains("quota") || low.contains("limit reached") || low.contains("limit has been")
    {
        "Your plan's limit is reached for now.".into()
    } else if low.contains("unauthorized") || low.contains("token expired") {
        "Your ChatGPT sign-in expired — connect again from the tray.".into()
    } else if low.contains("model") && low.contains("not") {
        "This model is not on your plan — trying again later.".into()
    } else {
        raw.chars().take(120).collect()
    }
}

/// Runs the request on the calling thread (a spawned std::thread in main.rs)
/// and feeds every event to `on`. Returning false from `on` stops reading and
/// cancels the stream quietly (island collapsed).
pub fn stream_chat(
    tokens: &Tokens,
    model: &str,
    history: &[Msg],
    mut on: impl FnMut(StreamEvent) -> bool,
) -> Result<(), String> {
    let agent = chat_agent();
    let mut req = agent
        .post(RESPONSES_URL)
        .set("authorization", &format!("Bearer {}", tokens.access))
        .set("originator", ORIGINATOR)
        .set("accept", "text/event-stream")
        .set("content-type", "application/json");
    if let Some(account) = &tokens.account_id {
        req = req.set("chatgpt-account-id", account);
    }
    let resp = match req.send_string(&responses_body(model, history).to_string()) {
        Ok(r) => r,
        Err(ureq::Error::Status(_, r)) => r,
        Err(_) => return Err("offline — can't reach ChatGPT".to_owned()),
    };
    if resp.status() != 200 {
        let status = resp.status();
        let snippet = resp.into_string().unwrap_or_default();
        let v: Value = serde_json::from_str(&snippet).unwrap_or(Value::Null);
        let raw = v["error"]["message"]
            .as_str()
            .or_else(|| v["message"].as_str())
            .or_else(|| v["detail"].as_str())
            .unwrap_or(&snippet);
        // 401/403 reads as an expired sign-in; anything else passes through.
        let msg = if status == 401 || status == 403 {
            user_error("unauthorized")
        } else {
            user_error(raw)
        };
        return Err(msg);
    }

    let mut reader = std::io::BufReader::new(resp.into_reader()); // into_reader is a plain Read
    let mut line = String::new();
    let mut got_text = false;
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break, // stream ended
            Ok(_) => {}
            Err(_) => {
                // A dropped connection after real text counts as done.
                if got_text {
                    let _ = on(StreamEvent::Done);
                    return Ok(());
                }
                return Err("The reply was cut off. Try again.".to_owned());
            }
        }
        let Some(rest) = line.strip_prefix("data:") else { continue };
        let rest = rest.trim();
        if rest.is_empty() || rest == "[DONE]" {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(rest) else { continue };
        match decode_event(&v) {
            None => {}
            Some(StreamEvent::Delta(d)) => {
                if !d.is_empty() && !on(StreamEvent::Delta(d)) {
                    return Ok(()); // cancelled by the UI
                }
                got_text = true;
            }
            Some(StreamEvent::Done) => {
                let _ = on(StreamEvent::Done);
                return Ok(());
            }
            Some(StreamEvent::Error(m)) => return Err(m),
        }
    }
    if got_text {
        let _ = on(StreamEvent::Done);
        Ok(())
    } else {
        Err("ChatGPT returned an empty reply.".to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_events_decode() {
        assert_eq!(
            decode_event(&serde_json::from_str::<Value>(
                r#"{"type":"response.output_text.delta","delta":"Hi"}"#
            ).unwrap()),
            Some(StreamEvent::Delta("Hi".into()))
        );
        assert_eq!(
            decode_event(&serde_json::from_str::<Value>(
                r#"{"type":"response.completed","response":{}}"#
            ).unwrap()),
            Some(StreamEvent::Done)
        );
        assert_eq!(
            decode_event(&serde_json::from_str::<Value>(
                r#"{"type":"response.failed","response":{"error":{"message":"quota exhausted"}}}"#
            ).unwrap()),
            Some(StreamEvent::Error("Your plan's limit is reached for now.".into()))
        );
        assert_eq!(
            decode_event(&serde_json::json!({"type":"response.created"})),
            None
        );
    }

    #[test]
    fn responses_body_shape() {
        let body = responses_body(
            "gpt-6-luna",
            &[
                Msg { role: Role::User, text: "hi".into() },
                Msg { role: Role::Assistant, text: "hey".into() },
            ],
        );
        assert_eq!(body["model"], "gpt-6-luna");
        assert_eq!(body["stream"], true);
        assert_eq!(body["store"], false);
        assert_eq!(body["input"][0]["role"], "user");
        assert_eq!(body["input"][0]["content"][0]["type"], "input_text");
        assert_eq!(body["input"][1]["role"], "assistant");
        assert_eq!(body["input"][1]["content"], "hey");
    }
}
