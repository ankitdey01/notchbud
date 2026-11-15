# notchbud — the notch with an assistant

A Windows notch island (pure Rust, Win32 + Direct2D/DComp) with a chat panel
that streams from your own Groq API key — no notchbud server.

## How it works

- Hover the notch → the island expands; type while it is open (a low-level
  keyboard hook feeds the input line, the window never activates) → answers
  stream into the island. Collapse cancels a running stream.
- **Enter Groq API Key** happens once in the island: paste a key from
  `https://console.groq.com/keys` (`gsk_...`). It is validated against
  Groq's model catalog (`https://api.groq.com/openai/v1/models`), then stored
  as a DPAPI-protected blob at `%APPDATA%\notchbud\auth.bin`, never plaintext.
  Chat goes to Groq's OpenAI-compatible chat API
  (`https://api.groq.com/openai/v1/chat/completions`) — every answer is
  billed to your own Groq key.
- The model is the first entry your key's model catalog offers; an empty
  catalog means no model is available (no hardcoded fallback).

## Companion script

`cargo run --example quota_spike` lists the key's models and sends one
streamed request — the record that decides what the key allows. Set
`GROQ_API_KEY` or save a key in the island first.

## Build

```
cargo build --release        # opt-level=z, LTO, stripped
```
