// chat.rs — the island chat: a Responses request to the public OpenAI API
// with the user's authorized ChatGPT-plan token, and the SSE stream decoded
// into StreamEvents. A turn counts only after `response.completed`; notch
// chat has no tool calls. Never any other billing path: plan-usage errors
// surface as-is, never a silent switch to API-key billing.

use std::io::BufRead;
use std::time::Duration;

use serde_json::{json, Value};
use ureq::Agent;

use super::auth::{Tokens, USER_AGENT};

pub const RESPONSES_URL: &str = "https://api.openai.com/v1/responses";

/// Everything chat sends is text; keep instructions plain.
const INSTRUCTIONS: &str = "You are notchbud, the tiny assistant inside a notch island on the desktop. Answer in one or two short plain-text sentences — no markdown, no lists.";

/// System prompt, with the user's name folded in: `instructions` rides every
/// request (Responses keeps no server-side state), so the model always knows
/// who it serves. The name is one user-supplied line (control characters are
/// rejected at save), quoted so it reads as data, never as an instruction.
pub fn instructions_for(user_name: Option<&str>) -> String {
    match user_name.map(str::trim).filter(|n| !n.is_empty()) {
        Some(name) => format!(
            "{INSTRUCTIONS} The person you are talking to is called \"{name}\"."
        ),
        None => INSTRUCTIONS.to_owned(),
    }
}

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

/// The Responses body for the island chat: the whole short history, nothing
/// stored server-side (`store: false`), streamed. `user_name` (the saved
/// onboarding name) is folded into `instructions` on every turn.
pub fn responses_body(model: &str, history: &[Msg], user_name: Option<&str>) -> Value {
    let input: Vec<Value> = history
        .iter()
        .map(|m| {
            json!({
                "role": match m.role { Role::User => "user", Role::Assistant => "assistant" },
                "content": m.text,
            })
        })
        .collect();
    json!({
        "model": model,
        "instructions": instructions_for(user_name),
        "input": input,
        "store": false,
        "stream": true,
    })
}

/// Documented plan-usage failure codes, mapped to slot-sized lines.
fn plan_error(code: &str) -> Option<String> {
    Some(match code {
        "subscription_sharing_user_not_eligible" => {
            "Your ChatGPT account can't share plan usage with apps.".into()
        }
        "subscription_sharing_usage_limit_exceeded" => {
            "Your plan's limit is reached for now. See ChatGPT Settings → Usage.".into()
        }
        "subscription_sharing_usage_unavailable" => {
            "Plan usage is temporarily unavailable. Try again later.".into()
        }
        "subscription_sharing_unsupported_capability" => {
            "That request isn't supported with plan usage.".into()
        }
        "subscription_sharing_route_not_supported" => {
            "This request route isn't supported with plan usage.".into()
        }
        "subscription_sharing_invalid_user" => {
            "Your ChatGPT account could not be verified. Sign in again.".into()
        }
        "chatpass_v2_scope_not_authorized" | "chatpass_v2_invalid_authorization_context" => {
            "This sign-in isn't authorized for plan usage. Connect again.".into()
        }
        "subscription_sharing_user_unavailable" => {
            "Account info is temporarily unavailable. Try again later.".into()
        }
        _ => return None,
    })
}

/// One `data:` line of the Responses event stream.
fn decode_event(v: &Value) -> Option<StreamEvent> {
    match v.get("type").and_then(Value::as_str) {
        Some("response.output_text.delta") => Some(StreamEvent::Delta(
            v.get("delta")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned(),
        )),
        Some("response.completed") => Some(StreamEvent::Done),
        Some("response.incomplete") => Some(StreamEvent::Error(
            "The reply was incomplete. Try again.".into(),
        )),
        Some("response.failed") => {
            let err = &v["response"]["error"];
            let code = err["code"].as_str().unwrap_or("");
            if let Some(mapped) = plan_error(code) {
                return Some(StreamEvent::Error(mapped));
            }
            let msg = err["message"]
                .as_str()
                .filter(|s| !s.is_empty())
                .unwrap_or("ChatGPT could not complete the request.");
            Some(StreamEvent::Error(user_error(msg)))
        }
        Some("error") => {
            let msg = v["error"]["message"]
                .as_str()
                .unwrap_or("ChatGPT could not complete the request.");
            Some(StreamEvent::Error(user_error(msg)))
        }
        _ => None,
    }
}

/// Friendly lines for the slot: quota exhausted / sign-in expired / offline.
fn user_error(raw: &str) -> String {
    let low = raw.to_ascii_lowercase();
    if low.contains("usage not included")
        || low.contains("usage_limit")
        || low.contains("rate limit")
        || low.contains("quota")
        || low.contains("limit reached")
        || low.contains("limit has been")
    {
        "Your plan's limit is reached for now.".into()
    } else if low.contains("unauthorized") || low.contains("token expired") {
        "Your ChatGPT sign-in expired — connect again from the tray.".into()
    } else if low.contains("model") && low.contains("not") {
        "This model is not on your plan — trying again later.".into()
    } else {
        raw.chars().take(140).collect()
    }
}

/// Runs the request on the calling thread (a spawned std::thread in main.rs)
/// and feeds every event to `on`. Returning false from `on` stops reading and
/// cancels the stream quietly (island collapsed).
pub fn stream_chat(
    tokens: &Tokens,
    model: &str,
    history: &[Msg],
    user_name: Option<&str>,
    mut on: impl FnMut(StreamEvent) -> bool,
) -> Result<(), String> {
    let agent = chat_agent();
    let req = agent
        .post(RESPONSES_URL)
        .set("authorization", &format!("Bearer {}", tokens.access))
        .set("accept", "text/event-stream")
        .set("content-type", "application/json");
    let resp = match req.send_string(&responses_body(model, history, user_name).to_string()) {
        Ok(r) => r,
        Err(ureq::Error::Status(_, r)) => r,
        Err(_) => return Err("offline — can't reach ChatGPT".to_owned()),
    };
    if resp.status() != 200 {
        let status = resp.status();
        let snippet = resp.into_string().unwrap_or_default();
        let v: Value = serde_json::from_str(&snippet).unwrap_or(Value::Null);
        // Direct-route admission may return {"detail":"..."} with no error
        // object; structured failures carry error.code.
        let code = v["error"]["code"].as_str().unwrap_or("").to_owned();
        if !code.is_empty() {
            if let Some(mapped) = plan_error(&code) {
                return Err(mapped);
            }
        }
        let raw = v["error"]["message"]
            .as_str()
            .or_else(|| v["detail"].as_str())
            .unwrap_or(&snippet);
        let msg = match status {
            401 => "Your ChatGPT sign-in was not accepted. Connect again from the tray.".to_owned(),
            403 => user_error(if raw.is_empty() { "not permitted" } else { raw }),
            503 => "ChatGPT plan routing is unavailable right now. Try again later.".to_owned(),
            _ => user_error(if raw.is_empty() {
                "ChatGPT could not complete the request."
            } else {
                raw
            }),
        };
        return Err(msg);
    }

    let mut reader = std::io::BufReader::new(resp.into_reader()); // into_reader is a plain Read
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
        if rest.is_empty() || rest == "[DONE]" {
            continue;
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
    // Success only after response.completed: an interrupted stream is a
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
                    r#"{"type":"response.output_text.delta","delta":"Hi"}"#
                )
                .unwrap()
            ),
            Some(StreamEvent::Delta("Hi".into()))
        );
        assert_eq!(
            decode_event(
                &serde_json::from_str::<Value>(r#"{"type":"response.completed","response":{}}"#)
                    .unwrap()
            ),
            Some(StreamEvent::Done)
        );
        assert_eq!(
            decode_event(&serde_json::from_str::<Value>(
                r#"{"type":"response.failed","response":{"error":{"code":"subscription_sharing_usage_limit_exceeded"}}}"#
            ).unwrap()),
            Some(StreamEvent::Error(
                "Your plan's limit is reached for now. See ChatGPT Settings → Usage.".into()
            ))
        );
        assert_eq!(
            decode_event(
                &serde_json::from_str::<Value>(r#"{"type":"response.incomplete","response":{}}"#)
                    .unwrap()
            ),
            Some(StreamEvent::Error(
                "The reply was incomplete. Try again.".into()
            ))
        );
        assert_eq!(
            decode_event(&serde_json::json!({"type":"response.created"})),
            None
        );
    }

    #[test]
    fn responses_body_shape() {
        let body = responses_body(
            "gpt-6.1-sol",
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
            Some("Alex"),
        );
        assert_eq!(body["model"], "gpt-6.1-sol");
        assert_eq!(body["stream"], true);
        assert_eq!(body["store"], false);
        assert_eq!(body["input"][0]["role"], "user");
        assert_eq!(body["input"][0]["content"], "hi");
        assert_eq!(body["input"][1]["role"], "assistant");
        assert_eq!(body["input"][1]["content"], "hey");
        // The saved name rides every request so the model knows its user.
        let instructions = body["instructions"].as_str().unwrap();
        assert!(instructions.contains("\"Alex\""), "{instructions}");
    }

    #[test]
    fn instructions_name_rules() {
        // Named, blank, and missing all behave: name quoted, no name, no name.
        let named = instructions_for(Some("Alex"));
        assert!(named.contains("called \"Alex\""), "{named}");
        assert!(named.starts_with(INSTRUCTIONS));
        assert_eq!(instructions_for(Some("   ")), INSTRUCTIONS);
        assert_eq!(instructions_for(None), INSTRUCTIONS);
        // One line, always: names cannot smuggle newlines past save.
        assert!(!named.chars().any(|c| c == '\n'));
    }
}
