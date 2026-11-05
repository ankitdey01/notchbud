# notchbud — the notch with an assistant

A Windows notch island (pure Rust, Win32 + Direct2D/DComp) with a chat panel
that streams from your own ChatGPT plan — no notchbud server, no API key.

## How it works

- Hover the notch → the island expands; type while it is open (a low-level
  keyboard hook feeds the input line, the window never activates) → answers
  stream into the island. Collapse cancels a running stream.
- **Sign in with ChatGPT** happens once: tray → *Connect ChatGPT* runs the
  OAuth device flow with the Codex CLI's public client_id; the approval page
  opens with the code in your clipboard. Tokens are stored as a DPAPI-protected
  blob at `%APPDATA%\notchbud\auth.bin`, never plaintext, and refreshed 5 minutes
  before expiry. Chat goes to `chatgpt.com/backend-api/codex/responses` marked
  `originator: notchbud` — every answer is billed to your plan's own allowance
  (free / Go / Plus limits are yours, and your plan carries them).
- The model is the first the plan's own listing offers, falling back to
  `gpt-6-luna`.

## Companion script

`cargo run --example quota_spike` performs the sign-in once and sends one
request, printing the HTTP status, the model list and the `x-codex-*` headers —
the record that decides which plan tiers notchbud claims to support.

## Build

```
cargo build --release        # opt-level=z, LTO, stripped
```
