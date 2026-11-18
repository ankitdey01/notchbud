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
pub const AUTH_REJECTED_MSG: &str = "Your Old Groq API key was rejected, enter a new one.";
/// The selected model is blocked for this project/key: the caller may try
/// the next catalog model once. Never clears the credential.
pub const MODEL_UNAVAILABLE_MSG: &str = "This model is not available — trying another helps.";

/// Everything chat sends is text; keep instructions plain.
const INSTRUCTIONS: &str = "You are notchbud, the tiny assistant inside a notch island on the desktop. Answer in one or two short plain-text sentences — no markdown, no lists.";

/// Separate title prompt: fired once per session, after the first reply
/// lands, so the Chats page can label it. Sees both sides — the user's
/// opening message and the assistant's reply — so the label reflects what
/// the chat is actually about. Short on purpose: 3-5 words, title only.
pub const TITLE_PROMPT: &str = "Create a short chat title of not more than 3 to 5 words for the conversation below. Reply with the title only, plain text, no quotes, no trailing punctuation.";

fn title_agent() -> Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(10))
        .timeout_read(Duration::from_secs(20))
        .user_agent(USER_AGENT)
        .build()
}

/// Messages for the title call: both sides of the first exchange. Empty
/// when there is nothing to title (the caller falls back to a snippet).
/// A missing/blank reply titles from the user's message alone.
fn title_messages(first_user_text: &str, assistant_reply: &str) -> Vec<Value> {
    let user = first_user_text.trim();
    if user.is_empty() {
        return Vec::new();
    }
    let mut messages = vec![
        json!({"role": "system", "content": TITLE_PROMPT}),
        json!({"role": "user", "content": user}),
    ];
    let reply = assistant_reply.trim();
    if !reply.is_empty() {
        messages.push(json!({"role": "assistant", "content": reply}));
    }
    messages
}

/// One-shot non-streaming title for a session. `None` on any failure —
/// the caller falls back to a local snippet so the list still labels.
pub fn fetch_title(
    api_key: &str,
    model: &str,
    first_user_text: &str,
    assistant_reply: &str,
) -> Option<String> {
    let messages = title_messages(first_user_text, assistant_reply);
    if messages.is_empty() {
        return None;
    }
    let body = json!({
        "model": model,
        "messages": messages,
        "stream": false,
    });
    let resp = title_agent()
        .post(GROQ_CHAT_URL)
        .set("authorization", &format!("Bearer {}", api_key.trim()))
        .set("accept", "application/json")
        .set("content-type", "application/json")
        .send_string(&body.to_string());
    let resp = match resp {
        Ok(r) => r,
        Err(ureq::Error::Status(_, r)) => r,
        Err(_) => return None,
    };
    if resp.status() != 200 {
        return None;
    }
    let v: Value = resp.into_string().ok().and_then(|s| serde_json::from_str(&s).ok())?;
    let raw = v["choices"][0]["message"]["content"].as_str()?.trim();
    if raw.is_empty() {
        return None;
    }
    // First line only, strip wrapping quotes, clamp to 8 words / 60 chars.
    let line = raw.lines().next().unwrap_or("").trim();
    let line = line.trim_matches(|c| c == '"' || c == '\'' || c == '`').trim();
    if line.is_empty() {
        return None;
    }
    let words: Vec<&str> = line.split_whitespace().take(8).collect();
    let mut out = words.join(" ");
    out = out.trim_matches(|c: char| c == '.' || c == '!' || c == '?' || c == '"' || c == '\'').trim().to_owned();
    if out.chars().count() > 60 {
        out = out.chars().take(60).collect();
    }
    if out.is_empty() { None } else { Some(out) }
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
    /// Wire string for history JSON and the chat body ("user"/"assistant").
    /// Single source so `ui_state` and `chat_body` can't drift apart.
    pub fn as_str(&self) -> &'static str {
        match self {
            Role::User => "user",
            Role::Assistant => "assistant",
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

/// Models with Groq's hosted web search (built-in `browser_search` tool,
/// https://console.groq.com/docs/tool-use/built-in-tools/browser-search).
/// The whole agentic loop runs server-side: the model searches when the
/// question needs current/external facts, then the final answer streams
/// back like a plain turn. Anything else chats exactly as before.
fn supports_browser_search(model: &str) -> bool {
    matches!(
        model.trim(),
        "openai/gpt-oss-20b" | "openai/gpt-oss-120b"
    )
}

/// The Groq chat body: system instructions + short history, streamed.
/// The saved display name rides the system prompt when set, so the model
/// knows who it is talking to. Nothing stored server-side; the whole
/// context rides every request. `image` (a PNG data URL from the camera
/// button) attaches to the latest user turn as an image_url part and is
/// strictly one-shot — history itself stays text-only.
pub fn chat_body(
    model: &str,
    user_name: &str,
    history: &[Msg],
    image: Option<&str>,
) -> Value {
    let user_name = user_name.trim();
    let mut instructions = INSTRUCTIONS.to_owned();
    if supports_browser_search(model) {
        instructions.push_str(" You have the browser_search tool; use it when the answer needs current or external facts.");
    }
    instructions.push_str(&format!(
        " The current local date and time is {}.",
        crate::user::local_now_text()
    ));
    if !user_name.is_empty() {
        instructions.push_str(&format!(" The user's name is {user_name}."));
    }
    // The camera shot rides the latest user turn only: text plus an
    // image_url part. Older turns stay plain text.
    let shot: Option<(usize, &str)> = image.filter(|s| !s.trim().is_empty()).and_then(|url| {
        history
            .iter()
            .rposition(|m| m.role == Role::User)
            .map(|i| (i, url))
    });
    let mut messages = Vec::with_capacity(history.len() + 1);
    messages.push(json!({"role": "system", "content": instructions}));
    for (i, m) in history.iter().enumerate() {
        if let Some((at, url)) = shot {
            if i == at {
                messages.push(json!({
                    "role": m.role.as_str(),
                    "content": [
                        {"type": "text", "text": m.text},
                        {"type": "image_url", "image_url": {"url": url}},
                    ],
                }));
                continue;
            }
        }
        messages.push(json!({
            "role": m.role.as_str(),
            "content": m.text,
        }));
    }
    let mut body = json!({
        "model": model,
        "messages": messages,
        "stream": true,
    });
    if supports_browser_search(model) {
        body["tools"] = json!([{"type": "browser_search"}]);
        body["tool_choice"] = json!("auto");
    }
    body
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
    // A finish reason alone never completes the turn: only `data:
    // [DONE]` does. Treating "stop"/"length" as Done masks a cut-off
    // stream (EOF without [DONE]) as success.
    None
}

/// Groq's hosted web search answers carry Exa-style citations (`【2†L26-L30】`).
/// The island slot is one or two plain-text sentences with no footnote UI,
/// so they render as junk glyphs — drop citation spans on ingest and keep
/// the sentence intact. Only spans whose contents hold the citation dagger
/// (`†`) go; ordinary bracketed text such as 【設定】 survives. An
/// unterminated `【` (marker split across stream chunks) is kept until its
/// `】` arrives, then the whole span goes if it cites.
pub fn strip_citation_markers(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('【') {
        match rest[start..].find('】') {
            Some(end) => {
                let close = start + end + '】'.len_utf8();
                // Citation spans carry † (e.g. 【2†L26-L30】); anything
                // else is ordinary bracketed text and stays.
                if rest[start..close].contains('†') {
                    out.push_str(&rest[..start]);
                    rest = &rest[close..];
                } else {
                    out.push_str(&rest[..close]);
                    rest = &rest[close..];
                }
            }
            None => {
                // Unterminated tail (marker split across stream chunks):
                // keep it whole; it drops on a later pass once `】`
                // arrives and only if it cites.
                out.push_str(rest);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

/// Friendly lines for the slot: quota / invalid key / unreachable.
fn user_error(raw: &str) -> String {
    let low = raw.to_ascii_lowercase();
    let has_any = |needles: &[&str]| needles.iter().any(|n| low.contains(n));
    if has_any(&[
        "invalid_api_key",
        "invalid api key",
        "incorrect api key",
        "unauthorized",
        "invalid_token",
    ]) {
        AUTH_REJECTED_MSG.into()
    } else if has_any(&[
        "usage not included",
        "usage_limit",
        "rate limit",
        "rate_limit",
        "quota",
        "limit reached",
        "tokens per",
    ]) {
        "Groq limit reached for now. Try again later.".into()
    } else if low.contains("model")
        && has_any(&["not", "decommissioned", "not found"])
    {
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
    user_name: &str,
    history: &[Msg],
    image: Option<&str>,
    on: impl FnMut(StreamEvent) -> bool,
) -> Result<(), String> {
    let agent = chat_agent();
    let req = agent
        .post(GROQ_CHAT_URL)
        .set("authorization", &format!("Bearer {}", api_key.trim()))
        .set("accept", "text/event-stream")
        .set("content-type", "application/json");
    let resp = match req.send_string(&chat_body(model, user_name, history, image).to_string()) {
        Ok(r) => r,
        Err(ureq::Error::Status(_, r)) => r,
        Err(_) => return Err("Couldn't reach Groq. Try again.".to_owned()),
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
    fn title_messages_pairs_user_and_reply() {
        let m = title_messages("plan a trip", "Sure — Kyoto in spring.");
        assert_eq!(m.len(), 3);
        assert_eq!(m[1]["role"], "user");
        assert_eq!(m[1]["content"], "plan a trip");
        assert_eq!(m[2]["role"], "assistant");
        assert!(m[2]["content"].as_str().unwrap().contains("Kyoto"));
    }

    #[test]
    fn title_messages_user_only_without_reply() {
        // A Done with no deltas still titles from the user's message.
        let m = title_messages("plan a trip", "   ");
        assert_eq!(m.len(), 2);
        assert_eq!(m[1]["role"], "user");
    }

    #[test]
    fn title_messages_empty_without_user_text() {
        assert!(title_messages("  ", "some reply").is_empty());
    }

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
            Some(StreamEvent::Error(AUTH_REJECTED_MSG.into()))
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
            "Alex",
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
            None,
        );
        assert_eq!(body["model"], "openai/gpt-oss-20b");
        assert_eq!(body["stream"], true);
        assert_eq!(body["messages"][0]["role"], "system");
        let sys = body["messages"][0]["content"].as_str().unwrap();
        assert!(sys.contains("notchbud"));
        assert!(
            sys.contains("Alex"),
            "the model must know who it is talking to, got: {sys}"
        );
        assert_eq!(body["messages"][1]["role"], "user");
        assert_eq!(body["messages"][1]["content"], "hi");
        assert_eq!(body["messages"][2]["role"], "assistant");
        assert_eq!(body["messages"][2]["content"], "hey");
    }

    #[test]
    fn chat_body_without_name_sends_base_instructions() {
        let body = chat_body(
            "llama-3.3-70b-versatile",
            "",
            &[Msg {
                role: Role::User,
                text: "hi".into(),
            }],
            None,
        );
        let sys = body["messages"][0]["content"].as_str().unwrap();
        assert!(
            !sys.contains("name"),
            "nameless users send the base instructions, nothing else, got: {sys}"
        );
        assert!(
            body.get("tools").is_none(),
            "unsupported models chat exactly as before, no tools attached"
        );
    }

    #[test]
    fn chat_body_attaches_browser_search_for_supported_models() {
        for model in ["openai/gpt-oss-20b", "openai/gpt-oss-120b"] {
            let body = chat_body(
                model,
                "",
                &[Msg {
                    role: Role::User,
                    text: "who won yesterday?".into(),
                }],
                None,
            );
            assert_eq!(body["tools"], json!([{"type": "browser_search"}]));
            assert_eq!(body["tool_choice"], "auto");
            let sys = body["messages"][0]["content"].as_str().unwrap();
            assert!(
                sys.contains("browser_search"),
                "the model must know search is available, got: {sys}"
            );
            assert!(
                sys.contains("local date and time is") && sys.contains("local time"),
                "every turn carries the local clock, got: {sys}"
            );
        }
    }

    #[test]
    fn chat_body_attaches_shot_to_latest_user_turn() {
        let url = "data:image/png;base64,AAA";
        let body = chat_body(
            "qwen/qwen3.8-27b",
            "",
            &[
                Msg { role: Role::User, text: "old q".into() },
                Msg { role: Role::Assistant, text: "old a".into() },
                Msg { role: Role::User, text: "what is this?".into() },
            ],
            Some(url),
        );
        // Older turns stay plain text.
        assert_eq!(body["messages"][1]["content"], "old q");
        assert_eq!(body["messages"][2]["content"], "old a");
        // Latest user turn carries text + image parts.
        let content = &body["messages"][3]["content"];
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[0]["text"], "what is this?");
        assert_eq!(content[1]["type"], "image_url");
        assert_eq!(content[1]["image_url"]["url"], url);
    }

    #[test]
    fn chat_body_without_shot_stays_text_only() {
        let body = chat_body(
            "openai/gpt-oss-20b",
            "",
            &[Msg { role: Role::User, text: "hi".into() }],
            Some("   "),
        );
        assert_eq!(body["messages"][1]["content"], "hi");
    }

    #[test]
    fn strip_citation_markers_drops_exa_footnotes() {
        assert_eq!(
            strip_citation_markers("Paris is the capital of France.【2†L26-L30】"),
            "Paris is the capital of France."
        );
        assert_eq!(
            strip_citation_markers("A【1†L1-L2】 mid【3†L4】 sentence."),
            "A mid sentence."
        );
        assert_eq!(strip_citation_markers("No markers here."), "No markers here.");
        // Ordinary bracketed text is not a citation and survives.
        assert_eq!(strip_citation_markers("設定【設定】done"), "設定【設定】done");
        // Unterminated marker (split stream chunk) survives until closed.
        assert_eq!(strip_citation_markers("Hi 【2†L2"), "Hi 【2†L2");
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
