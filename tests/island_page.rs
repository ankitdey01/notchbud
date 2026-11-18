// tests/island_page.rs — contract tests for the embedded island page (ui/).
//
// The island page is baked into the binary (include_str!) and driven only via
// ExecuteScript: a JS-level breakage (a duplicated line silently nesting the
// IIFE body into a never-called function, a missing element id, an external
// link navigating the WebView away, an intent without a Rust handler) shows up
// on-device as a dead island with no Rust-side error. Pin the contracts here
// in plain source terms so edits fail here instead of on a user's screen.

use std::path::PathBuf;

fn ui_file(name: &str) -> String {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("ui");
    p.push(name);
    std::fs::read_to_string(&p).unwrap_or_else(|_| panic!("missing ui/{name}"))
}

fn src_file(name: &str) -> String {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("src");
    p.push(name);
    std::fs::read_to_string(&p).unwrap_or_else(|_| panic!("missing src/{name}"))
}

fn js() -> String {
    ui_file("app.js")
}

fn html() -> String {
    ui_file("index.html")
}

/// A duplicated content line silently nests the rest of the IIFE body into
/// a never-called function: still valid syntax (node --check passes), but
/// `window.notchbud` is never assigned and no listener ever attaches — a dead
/// island with no error anywhere. That exact incident is what this pins.
/// (Bare delimiter lines like `}` legitimately repeat, so only lines with
/// actual content count.)
#[test]
fn no_duplicated_header_lines() {
    let mut prev = String::new();
    for (n, line) in js().lines().enumerate() {
        let t = line.trim();
        if !t.is_empty() && t == prev && t.chars().any(|c| c.is_alphanumeric()) {
            panic!("ui/app.js:{}: duplicated line nests the IIFE body: {t}", n + 1);
        }
        prev = t.to_string();
    }
}

/// Every element id the script looks up must exist in the page, or the lookup
/// is a silent null and that whole flow (key entry, send, settings) is dead.
#[test]
fn js_element_ids_exist_in_html() {
    let js = js();
    let html = html();
    let mut ids: Vec<String> = Vec::new();
    let mut rest = js.as_str();
    while let Some(i) = rest.find("el(\"") {
        let after = &rest[i + 4..];
        if let Some(end) = after.find('"') {
            ids.push(after[..end].to_owned());
            rest = &after[end + 1..];
        } else {
            break;
        }
    }
    assert!(!ids.is_empty(), "no el() lookups found — scanner broke");
    for id in &ids {
        assert!(
            html.contains(&format!("id=\"{id}\"")),
            "ui/app.js looks up #{id}, but ui/index.html has no such id"
        );
    }
}

/// No external navigation inside the embedded page: one click on a raw
/// `http` link navigates the WebView away from the island page and every
/// Rust push/intent silently no-ops until restart. External URLs must go
/// through Rust (system browser), never an anchor.
#[test]
fn island_html_has_no_external_links() {
    assert!(
        !html().contains("href=\"http"),
        "embedded page must not link out: open external URLs via Rust"
    );
}

/// `type: "xxx"` values the page can post.
fn posted_intents(source: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut rest = source;
    while let Some(i) = rest.find("type: \"") {
        let after = &rest[i + 7..];
        if let Some(end) = after.find('"') {
            out.push(after[..end].to_owned());
            rest = &after[end + 1..];
        } else {
            break;
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Every intent the page posts must have a Rust handler arm, or the click
/// silently does nothing.
#[test]
fn posted_intents_have_rust_handlers() {
    let posted = posted_intents(&js());
    assert!(!posted.is_empty(), "no intents found — scanner broke");
    let main = src_file("main.rs");
    let span = main
        .find("fn handle_webmsg")
        .and_then(|s| {
            main[s..]
                .find("\nfn ")
                .map(|e| main[s..s + e].to_string())
        })
        .expect("handle_webmsg not found");
    for intent in &posted {
        assert!(
            span.contains(&format!("Some(\"{intent}\")")),
            "page posts {intent:?}, but handle_webmsg has no arm for it"
        );
    }
}

/// Chats page: one linear list — every session a row with its title and
/// last message, plus a New-chat starter. Rows post open_session; Rust owns
/// the restore (see main.rs open_session arm).
#[test]
fn chats_page_lists_title_plus_last_message() {
    let h = html();
    let chats = h
        .find("id=\"screen-chats\"")
        .and_then(|s| h[s..].find("</section>").map(|e| h[s..s + e].to_string()))
        .expect("screen-chats not found");
    for id in ["id=\"chats-empty\"", "id=\"chats-list\""] {
        assert!(
            chats.contains(id),
            "Chats page must own {id} (empty state + session list)"
        );
    }
    let js = js();
    let span = js
        .find("function renderChats")
        .and_then(|s| {
            js[s..]
                .find("\n  function ")
                .map(|e| js[s..s + e].to_string())
        })
        .expect("renderChats not found");
    let f: String = span.chars().filter(|c| !c.is_whitespace()).collect();
    assert!(
        f.contains("chat-title") && f.contains("chat-preview"),
        "rows must show the title with the last message under it"
    );
    assert!(
        f.contains("type:\"open_session\""),
        "tapping a row must post the open_session intent"
    );
}

/// Settings keeps the clean profile card: no inline name editor. A small
/// pencil beside the name jumps to the shared name step (edit_name intent,
/// owned by Rust like every other nav).
#[test]
fn settings_edits_name_via_shared_name_step() {
    let h = html();
    let settings = h
        .find("id=\"screen-settings\"")
        .and_then(|s| h[s..].find("</section>").map(|e| h[s..s + e].to_string()))
        .expect("screen-settings not found");
    assert!(
        settings.contains("id=\"settings-name-edit\""),
        "settings needs a small edit affordance beside the name"
    );
    for gone in ["id=\"name-edit\"", "id=\"settings-name-input\""] {
        assert!(
            !settings.contains(gone),
            "settings must not inline a name editor ({gone} found) — the shared name step owns that"
        );
    }
    let js = js();
    assert!(
        !js.contains("settings-name-input"),
        "page script must not drive an inline settings editor — edit_name jumps to the shared step"
    );
    assert!(
        js.contains("type: \"edit_name\""),
        "edit affordance must post the edit_name intent"
    );
    let render = js
        .find("function renderSettings")
        .and_then(|s| {
            js[s..]
                .find("\n  function ")
                .map(|e| js[s..s + e].to_string())
        })
        .expect("renderSettings not found");
    let rf: String = render.chars().filter(|c| !c.is_whitespace()).collect();
    assert!(
        rf.contains("settingsNameEdit.style.display=connected"),
        "pencil shows only when signed in — signed-out settings has no name to edit"
    );
}

/// Prompt sends are up arrows that ride disabled (gray) until there is
/// text, and while Rust streams a reply. Blue = ready, gray = nothing
/// to send. Guards the ExecuteScript path too (key() mutates values
/// without firing input events).
#[test]
fn prompt_send_arrow_disables_when_empty_or_streaming() {
    let h = html();
    for id in ["id=\"home-send\"", "id=\"chat-send\""] {
        let btn = h
            .find(id)
            .and_then(|s| h[s..].find("</button>").map(|e| h[s..s + e].to_string()))
            .unwrap_or_else(|| panic!("{id} button not found"));
        assert!(
            btn.contains("M6 9.83"),
            "{id} must use the up-arrow icon, not the paper plane"
        );
        assert!(
            !btn.contains("M4.934"),
            "{id} must not keep the paper-plane icon"
        );
        assert!(
            btn.contains("disabled"),
            "{id} must start disabled so first paint is gray"
        );
    }
    let js = js();
    let f: String = js.chars().filter(|c| !c.is_whitespace()).collect();
    assert!(
        f.contains("for(constpofPROMPTS)"),
        "send enablement must loop the shared PROMPTS table, not per-prompt branches"
    );
    assert!(
        f.contains("p.send.disabled=isStreaming||awaitingAck||!(p.input&&p.input.value.trim())"),
        "each send must disable while streaming, while a submit awaits ack, or when its box is empty"
    );
    assert!(
        f.contains("isStreaming=!!s.streaming"),
        "render() must track Rust's streaming flag for the send buttons"
    );
    assert!(
        f.contains("addEventListener(\"input\",refreshSend)"),
        "typing must refresh the send buttons via the input event"
    );
    let css_file = ui_file("styles.css");
    let c: String = css_file.chars().filter(|c| !c.is_whitespace()).collect();
    assert!(
        c.contains(".send:disabled{"),
        "disabled sends need their own gray rule, not the faded-blue default"
    );
}

/// Pre-reply state is shimmer "Thinking" text with no bubble — never the
/// old "…" bubble. The bubble appears with the first arriving text.
#[test]
fn chat_thinking_shimmers_without_bubble() {
    let js = js();
    let span = js
        .find("function renderChat")
        .and_then(|s| {
            js[s..]
                .find("\n  function ")
                .map(|e| js[s..s + e].to_string())
        })
        .expect("renderChat not found");
    let f: String = span.chars().filter(|c| !c.is_whitespace()).collect();
    assert!(
        f.contains("turn-thinking") && f.contains("\"Thinking\""),
        "streaming with no text yet must render shimmer Thinking text"
    );
    assert!(
        !span.contains('…'),
        "the … placeholder bubble must be gone"
    );
    let css = ui_file("styles.css");
    let c: String = css.chars().filter(|c| !c.is_whitespace()).collect();
    assert!(
        c.contains(".turn-thinking{") && c.contains("thinking-shimmer"),
        "Thinking needs its left-to-right shimmer sweep"
    );
    assert!(
        c.contains("background-clip:text"),
        "shimmer must clip the sweep to the text itself"
    );
}

/// Chat feed copies the bubble look: user turns hug the right in the
/// global blue, assistant turns stretch full-width in dark #1c1c1e —
/// plain text inside, no "› " prefix.
#[test]
fn chat_feed_uses_blue_and_dark_bubbles() {
    let js = js();
    let span = js
        .find("function renderChat")
        .and_then(|s| {
            js[s..]
                .find("\n  function ")
                .map(|e| js[s..s + e].to_string())
        })
        .expect("renderChat not found");
    assert!(
        !span.contains("›"),
        "user turns must render plain text — no \"› \" prefix inside bubbles"
    );
    let css = ui_file("styles.css");
    let c: String = css.chars().filter(|c| !c.is_whitespace()).collect();
    assert!(
        c.contains(".turn-user{") && c.contains("var(--cb-primary)"),
        "user bubble must use the global blue, not white"
    );
    assert!(
        c.contains("align-self:flex-end"),
        "user bubble must hug the right edge like the inspiration"
    );
    assert!(
        c.contains(".turn-assistant{") && c.contains("#1c1c1e"),
        "assistant bubble must be the dark fill from the inspiration"
    );
}

/// Chats rows carry a trash affordance on the right that pops a blank
/// confirm page (big question + the chat's title + Yes/No) instead of
/// deleting inline. Yes is red, No is blue; Esc counts as No.
#[test]
fn chats_rows_offer_delete_with_confirm_page() {
    let h = html();
    let confirm = h
        .find("id=\"screen-confirm\"")
        .and_then(|s| h[s..].find("</section>").map(|e| h[s..s + e].to_string()))
        .expect("screen-confirm not found");
    assert!(
        confirm.contains("Are you sure you want to delete this chat?"),
        "confirm page must ask the big question in plain words"
    );
    for id in ["id=\"confirm-name\"", "id=\"confirm-yes\"", "id=\"confirm-no\""] {
        assert!(confirm.contains(id), "confirm page must own {id}");
    }
    assert!(
        confirm.contains("btn-destructive") && confirm.contains("btn-primary"),
        "Yes must ride the red Coss variant, No the blue one"
    );
    let js = js();
    let span = js
        .find("function renderChats")
        .and_then(|s| {
            js[s..]
                .find("\n  function ")
                .map(|e| js[s..s + e].to_string())
        })
        .expect("renderChats not found");
    let f: String = span.chars().filter(|c| !c.is_whitespace()).collect();
    assert!(
        f.contains("chat-open") && f.contains("chat-del"),
        "rows must split into an open block plus a right-side trash"
    );
    assert!(
        f.contains("type:\"ask_delete\""),
        "trash must pop the confirm page via the ask_delete intent"
    );
    assert!(
        js.contains("type: \"confirm_delete\"") && js.contains("type: \"cancel_delete\""),
        "Yes/No must post confirm_delete / cancel_delete"
    );
    assert!(
        js.chars()
            .filter(|c| !c.is_whitespace())
            .collect::<String>()
            .contains("lastScreen===\"confirm\""),
        "Esc on the confirm page must count as No, not a plain nav-back"
    );
    let css = ui_file("styles.css");
    let c: String = css.chars().filter(|c| !c.is_whitespace()).collect();
    assert!(
        c.contains(".chat-del{") && c.contains(".chat-del:hover{"),
        "trash needs its own rest/hover rule — white normally, red on hover"
    );
    assert!(
        c.contains("color:#ff5a52"),
        "trash hover must turn red"
    );
    assert!(
        !confirm.contains("data-nav"),
        "confirm page is a blank island — no nav links, just Yes/No"
    );
    assert!(
        js.chars()
            .filter(|c| !c.is_whitespace())
            .collect::<String>()
            .contains("name===\"confirm\""),
        "confirm page must hide the topbar like signin does"
    );
}

/// Arrow keys move the caret in the prompt. The window never activates,
/// so the hook delivers arrows as distinct non-Unicode codes (never native
/// keypresses) — Rust must forward them (they yield no text, otherwise they
/// leak to the app behind the island) and the page must treat them as
/// motion, never as %&'( input. Both prompts share one PROMPTS table + one
/// wiring loop: a single prompt logic, never two clones to drift apart.
#[test]
fn prompt_arrows_move_caret_and_wiring_is_shared() {
    let main = src_file("main.rs");
    assert!(
        main.contains("KEY_LEFT")
            && main.contains("KEY_RIGHT")
            && main.contains("0x110010"),
        "hook must forward arrows as distinct non-Unicode codes past the Unicode range, not 37-40 that collide with '%'"
    );
    assert!(
        !main.contains("8 | 13 | 27 | 37 | 38 | 39 | 40"),
        "arrows must not share codes with printable scalars"
    );
    let js = js();
    let f: String = js.chars().filter(|c| !c.is_whitespace()).collect();
    assert!(
        f.contains("constPROMPTS=[{input:homeInput,send:homeSend,cam:homeCam,shot:homeShot,img:homeShotImg,skel:homeShotSkel,shotX:homeShotX},{input:chatInput,send:chatSend,cam:chatCam,shot:chatShot,img:chatShotImg,skel:chatShotSkel,shotX:chatShotX},]"),
        "both prompts must pair through one PROMPTS table (input + send + camera + preview)"
    );
    assert!(
        f.contains("functionmoveCaret(input,code)") && f.contains("KEY_LEFT") && f.contains("isArrow(code)"),
        "page must own a moveCaret helper fed by the distinct arrow codes"
    );
    assert!(
        f.contains("setSelectionRange"),
        "arrow handling must move the caret, not edit text"
    );
    assert!(
        !js.contains("homeSend.addEventListener") && !js.contains("chatSend.addEventListener"),
        "send buttons must wire through the shared loop, not per-prompt clones"
    );
}

/// Camera button beside each send: Pikaicons camera captures the screen,
/// the thumbnail previews in the typing pill, and it sends with the text.
/// Clear removes it; submits always carry text (image-only sends stay out).
#[test]
fn camera_captures_previews_and_sends_with_text() {
    let h = html();
    for id in ["id=\"home-cam\"", "id=\"chat-cam\""] {
        let btn = h
            .find(id)
            .and_then(|s| h[s..].find("</button>").map(|e| h[s..s + e].to_string()))
            .unwrap_or_else(|| panic!("{id} button not found"));
        assert!(
            btn.contains("M2 11c0-2.8"),
            "{id} must use the Pikaicons camera, not a hand-drawn SVG"
        );
    }
    for id in [
        "id=\"home-shot\"",
        "id=\"chat-shot\"",
        "id=\"home-shot-img\"",
        "id=\"chat-shot-img\"",
        "id=\"home-shot-skel\"",
        "id=\"chat-shot-skel\"",
        "id=\"home-shot-x\"",
        "id=\"chat-shot-x\"",
    ] {
        assert!(h.contains(id), "prompt preview needs {id} in both askboxes");
    }
    let js = js();
    let f: String = js.chars().filter(|c| !c.is_whitespace()).collect();
    assert!(
        js.contains("type: \"take_shot\"") && js.contains("type: \"clear_shot\""),
        "camera and remove must post take_shot / clear_shot"
    );
    assert!(
        f.contains("functionrenderShot(s)") && f.contains("s.pendingImage"),
        "page must render the pendingImage preview into the pill"
    );
    let css = ui_file("styles.css");
    let c: String = css.chars().filter(|c| !c.is_whitespace()).collect();
    assert!(
        c.contains(".shot-wrap{") && c.contains(".shot-img{") && c.contains(".shot-skel{"),
        "preview needs its pill thumbnail + skeleton styles"
    );
    assert!(
        c.contains(".shot-skel{") && c.contains("thinking-shimmer"),
        "the skeleton must reuse the shimmer sweep, not invent a new animation"
    );
}

/// A sent screenshot renders as a thumbnail directly above its bubble —
/// plain turns render text only, same as before.
#[test]
fn chat_shot_renders_thumbnail_above_its_bubble() {
    let js = js();
    let span = js
        .find("function renderChat")
        .and_then(|s| {
            js[s..]
                .find("\n  function ")
                .map(|e| js[s..s + e].to_string())
        })
        .expect("renderChat not found");
    let f: String = span.chars().filter(|c| !c.is_whitespace()).collect();
    assert!(
        f.contains("turn-shot") && f.contains("t.image"),
        "turns carrying an image must render the thumbnail above the bubble"
    );
    let css = ui_file("styles.css");
    let c: String = css.chars().filter(|c| !c.is_whitespace()).collect();
    assert!(
        c.contains(".turn-shot{") && c.contains("align-self:flex-end"),
        "the thumbnail must hug the right edge over its bubble"
    );
}

/// Overlong prompts must show their newest tail. Hook-driven typing sets
/// .value programmatically, which never scrolls the field like native typing
/// does — without a manual scroll the newest text hides on the right while
/// old text sits visible on the left.
#[test]
fn long_prompts_scroll_newest_text_into_view() {
    let js = js();
    let f: String = js.chars().filter(|c| !c.is_whitespace()).collect();
    assert!(
        f.contains("functionrevealCaret(input)"),
        "page must own a caret-reveal helper for programmatic edits"
    );
    assert!(
        f.contains("input.scrollLeft=input.scrollWidth"),
        "overlong input must scroll to its tail so the newest text shows"
    );
    for name in ["function editText", "function moveCaret"] {
        let span = js
            .find(name)
            .and_then(|s| {
                js[s..]
                    .find("\n  function ")
                    .map(|e| js[s..s + e].to_string())
            })
            .unwrap_or_default();
        assert!(
            span.contains("revealCaret"),
            "{name} must reveal the caret so typed text never hides on the right"
        );
    }
}

/// Renaming starts from the saved name: opening the step prefills the
/// input with the current name (cursor to end), never a blank box.
#[test]
fn home_name_step_prefills_current_name() {
    let js = js();
    let span = js
        .find("function renderHome")
        .and_then(|s| {
            js[s..]
                .find("\n  function ")
                .map(|e| js[s..s + e].to_string())
        })
        .expect("renderHome not found");
    let f: String = span.chars().filter(|c| !c.is_whitespace()).collect();
    assert!(
        f.contains("homeName.value=current"),
        "opening the name step must prefill the saved name for editing"
    );
    assert!(
        f.contains("setSelectionRange(homeName.value.length,homeName.value.length)"),
        "cursor must land at the end of the prefilled name, not the start"
    );
}

/// Home shows the name box OR the chat box, never both: with no saved
/// name only "What should we call you?" may show (mirrors SignIn owning
/// the frame when signed out — no topbar, no greeting, one centered box).
/// Rust decides via `needsName`; renderHome must apply that one flag to
/// the navbar, greeting, and both boxes.
#[test]
fn home_name_and_chat_boxes_never_overlap() {
    let h = html();
    let home = h
        .find("id=\"screen-home\"")
        .and_then(|s| h[s..].find("<section").map(|e| h[s..s + e].to_string()))
        .expect("screen-home not found");
    for id in ["id=\"name-entry\"", "id=\"home-ask\"", "id=\"home-greet\""] {
        assert!(
            home.contains(id),
            "Home must own all three to toggle them ({id} missing from screen-home)"
        );
    }
    let js = js();
    let span = js
        .find("function renderHome")
        .and_then(|s| {
            js[s..]
                .find("\n  function ")
                .map(|e| js[s..s + e].to_string())
        })
        .expect("renderHome not found");
    let f: String = span.chars().filter(|c| !c.is_whitespace()).collect();
    assert!(
        f.contains("nameEntry.hidden=!needsName"),
        "renderHome must hide the name box once answered"
    );
    assert!(
        f.contains("homeAsk.hidden=needsName"),
        "renderHome must hide the chat box until the name is answered — the two must never overlap"
    );
    assert!(
        f.contains("topbar.hidden=needsName"),
        "renderHome must hide the navbar until the name is answered — signin shows no navbar either"
    );
    assert!(
        f.contains("homeGreet.hidden=needsName"),
        "renderHome must hide the greeting until the name is answered — only the question may show"
    );
    assert!(
        f.contains("classList.toggle(\"name-only\",needsName)"),
        "renderHome must center the lone name box like the signin key entry"
    );
}
