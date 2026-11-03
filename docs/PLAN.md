# dyland v1 — The Notch With an Assistant

Confirmed intent (interview-me restate, user-confirmed):

- **Outcome:** hover the notch → lightweight chat panel streams answers inside the island. One-time **Sign in with ChatGPT** device-code OAuth; all chat consumes **only the user's own ChatGPT plan allowance**. Pure client, no dyland server, no API key.
- **User:** Windows users with any chat.ai account (free / Go / Plus).
- **Constraint:** the hover swell→pop→collapse behavior and all visuals are **fixed and final**; no UI changes unless told. Pure Rust, small binary opt-level=z.
- **Out of scope v1:** Claude connector, model pickers, accounts/backend/payments, pro tier, cookie/web-session reversing.
- **Risk accepted:** rides OpenAI tolerance of originator-tagged third-party clients on the public Codex client_id.

## Technique (ported from Freebuff `CodebuffAI/freebuff`, sdk/src/chatgpt.ts)

1. OAuth device flow at `auth.openai.com/api/accounts/deviceauth/usercode` → `…/deviceauth/token`, using the Codex CLI's public client_id `app_EMoamEEZ73f0CkXaXp7hrann` (not a secret, theirs). User approves at `https://auth.openai.com/codex/device`.
2. Chat requests → `POST https://chatgpt.com/backend-api/codex/responses` (Responses API, streaming) with `Authorization: Bearer <access>`, `ChatGPT-Account-Id: <id>`, `originator: dyland`.
3. Translate island chat messages → Responses `input` items; read only `response.output_text.delta` + `response.completed` from the SSE stream (no tool calls exist in notch chat).
4. Refresh access tokens 5 min before expiry; re-sign-in when refresh fails permanently.
5. Default model `gpt-6-luna`; try plan's own model listing first, fall back to `gpt-6-sol`/`gpt-6-astra`/`gpt-6-luna` (Freebuff's defaults).

## Milestone 0 — quota spike (before any code in dyland)

Standalone 50-line script (Rust bin or Node) that runs the device flow once and sends one request against a **free/Go** account.

- Records: HTTP status, returned model list, actual quota/limits response.
- **This decides the whole positioning.** If free accounts 403 → product claims change to "any paid plan"; still ship, but the onboarding story and marketing re-aim.

## Milestone 1 — connector core (new `src/connector/`)

New dependencies, total 3: `ureq` (sync HTTP + SSE reads, no tokio — buys binary size), `serde`, `serde_json`.

- `connector/auth.rs` — device flow start/poll/token store. Refresh token at rest via **DPAPI** (`CryptProtectData` → `%APPDATA%\dyland\auth.bin`). Never plaintext.
- `connector/chat.rs` — Responses body builder + SSE line parser → `StreamEvent::Delta(String) | Done | Error(String)`.
- Threading: chat runs on a spawned `std::thread`; events post into the UI loop via `PostMessage` custom message (fits the WM_TIMER message-pump pattern already in `main.rs`). No async runtime anywhere.

## Milestone 2 — chat panel inside the frozen island

- `state.rs`: add `ChatState { input: String, stream: String, history: Vec<(role, String)>, status }`. Geometry, springs, scale choreography: **untouched**.
- `render.rs`: the greeting slot (`draw_expanded_content`, line ~686) becomes the chat surface — same DirectWrite layout path, same `content_alpha` fade, same fonts. Silhouette identical.
- **Keyboard input** is the one functional addition: the window is `WS_EX_NOACTIVATE`, so hover alone never grants keystrokes. Simplest working scheme: hold the existing expanded state as the gate, capture keys via a low-level keyboard hook (`SetWindowsExHook`-style `WH_KEYBOARD`) that posts chars to the pump; input line + caret drawn in D2D. Visible behavior unchanged from the fixed UI.
- Multi-turn: send the whole short history each call (still cheap after translation); collapse on leave may cancel the stream or let it finish quietly — pick one, default cancel.

## Milestone 3 — onboarding + tray

- `tray.rs` menu: **Connect ChatGPT** (open `auth.openai.com/codex/device` in the browser, clipboard the user code — the island greeting slot shows "code: XXXX-XXXX" briefly; content in the frozen slot, layout untouched), **Disconnect** (delete auth.bin), **Quit**.
- Error lines reuse the same slot: quota exhausted / sign-in expired / offline.

## Milestone 4 — ship

- Default model honesty note in README (limits are yours, your plan's pipe).
- Size check (`opt-level=z`, strip): expect +~1.5 MB for ureq+TLS. If painful, `rustls` with a smaller root set or let ureq use its default and accept it.
- One-time Pro unlock goes to a later version; nothing here pre-bakes it.

## Known risks

1. **Quota on free/Go tiers unverified** — Milestone 0 exists for this.
2. **Client_id / endpoint is OpenAI's CLI surface**; a policy change breaks the connector. Mitigate by keeping all of it in `connector/`, authored to fail loudly, never to degrade into something that looks like a bug.
3. **originator: dyland** — identify honestly, as Freebuff did; the account carries the plan, our client carries the name.
