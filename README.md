# notchbud — the notch with an assistant

A Windows notch island (pure Rust, Win32 + Direct2D/DComp) with a chat panel
that streams from your own ChatGPT plan — no notchbud server, no API key.

## How it works

- Hover the notch → the island expands; type while it is open (a low-level
  keyboard hook feeds the input line, the window never activates) → answers
  stream into the island. Collapse cancels a running stream.
- **Sign in with ChatGPT** happens once: tray → *Connect ChatGPT* runs the
  OAuth authorization-code + PKCE flow against a local loopback callback;
  the approval page opens in your browser. Tokens are stored as a DPAPI-protected
  blob at `%APPDATA%\notchbud\auth.bin`, never plaintext, and refreshed 5 minutes
  before expiry. Chat goes to the public Responses API (`https://api.openai.com/v1/responses`)
  with your plan token — every answer is billed to your plan's own allowance.
- The model is the first entry your plan's own model catalog offers; an empty
  catalog means no model is available (no hardcoded fallback).

## Companion script

`cargo run --example quota_spike` performs the sign-in once and sends one
request, printing the token expiry, the plan flag, and the model catalog —
the record that decides what the account's plan allows.

## Build

```
cargo build --release        # opt-level=z, LTO, stripped
```
