# notchbud — the notch with an assistant

A Windows notch island (pure Rust, Win32 + Direct2D/DComp) with a chat panel
that streams from your own Groq API key — no NotchBud server.

## How it works

- Hover the notch → the island expands; type while it is open (a low-level
  keyboard hook feeds the input line, the window never activates) → answers
  stream into the island. Collapse cancels a running stream.
- **Enter Groq API Key** happens once in the island: paste a key from
  `https://console.groq.com/keys` (`gsk_...`). It is validated against
  Groq's model catalog (`https://api.groq.com/openai/v1/models`), then stored
  as a DPAPI-protected blob at `%APPDATA%\\notchbud\\auth.bin`, never plaintext.
  Chat goes to Groq's OpenAI-compatible chat API
  (`https://api.groq.com/openai/v1/chat/completions`) — every answer is
  billed to your own Groq key.
- The model is the first entry your key's model catalog offers; an empty
  catalog means no model is available (no hardcoded fallback).

## Requirements

- Windows x64.
- Microsoft Edge WebView2 Runtime.
- Your own Groq API key. NotchBud does not provide ChatGPT subscription access,
  free ChatGPT tokens, or a NotchBud-hosted model endpoint.

## Build from source

```powershell
cargo fmt --all -- --check
cargo test --locked
cargo build --release --locked
choco install innosetup --yes
& "C:\\Program Files (x86)\\Inno Setup 6\\ISCC.exe" installer\\notchbud.iss
```

The per-user installer is written to
`installer/Output/Notchbud-Setup-0.1.2.exe`.

## Create a public preview release

Open **GitHub → Actions → Windows release → Run workflow**. The workflow
checks formatting and tests, builds the release binary and per-user installer,
runs the installer checks, writes a SHA-256 checksum, and publishes a
`v0.1.2` prerelease. The installer is not published until that workflow
finishes successfully.

## Companion script

`cargo run --example quota_spike` lists the key's models and sends one
streamed request — the record that decides what the key allows. Set
`GROQ_API_KEY` or save a key in the island first.
