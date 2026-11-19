# Contributing to notchbud

Thanks for helping. A few things are specific to this repo, so read this first.

## Setup

- Windows 64 bit. Building and running assume Windows.
- Rust stable toolchain. CI uses stable on `windows-latest` (see `.github/workflows/ci.yml`).
- Microsoft Edge WebView2 Runtime to run the app.
- Inno Setup 6 only if you build the installer.
- A Groq API key (`gsk_...`) only if you test live chat.

```powershell
git clone https://github.com/ankitdey01/notchbud.git
cd notchbud
cargo test
```

## Running

```powershell
Start-Process powershell -Verb RunAs -ArgumentList '-NoExit', '-Command', "Set-Location -LiteralPath '$((Get-Location).Path)'; cargo run"
```

Always run it this way. Plain `cargo run` leaves the app broken: tray registration is denied, typing does nothing, and links do not open. The elevated shell is the supported test path.

Debug builds keep a console window for log output. Release builds hide it (see `src/main.rs`). For a release binary:

```powershell
cargo build --release
```

Live chat needs a key. Either set `GROQ_API_KEY` or save a key in the island first. `examples/quota_spike.rs` shows the exact fallback order and is a good smoke test:

```powershell
$env:GROQ_API_KEY="gsk_..."
cargo run --example quota_spike
```

## Before you push

```powershell
cargo fmt --all
cargo test
```

`cargo test` is what CI runs on every push and PR. Keep PRs focused, one change per PR, and describe how you tested it.

## UI changes

- The island viewport is small. Always verify visual changes at real content size, never in a tall desktop viewport. Anchoring bugs only show at true height.
- The island sizing constants live in `src/state.rs`. Read them before touching `ui/`.
- Keep chat bubbles hugging their text and single line text roomy enough that descenders are not clipped.

## Installer

The version is defined in two places: `Cargo.toml` and `installer/notchbud.iss` (`MyAppVersion`). Keep them in sync.

```powershell
cargo build --release
& "C:\Program Files (x86)\Inno Setup 6\ISCC.exe" installer\notchbud.iss
```

The setup exe lands in `installer/Output/`. The install is per user with no admin rights.

## Releases

Releases are maintainer driven through `.github/workflows/release.yml`:

1. Update the version in `Cargo.toml` and in `installer/notchbud.iss`.
2. Commit and push a tag like `v0.1.3`. The tag must match `Cargo.toml` or the workflow fails.
3. The workflow runs tests, builds the release binary, builds the installer, and publishes both to GitHub Releases.
