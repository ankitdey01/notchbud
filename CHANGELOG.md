# Changelog

All notable changes to notchbud are recorded here, newest first.
Release notes with downloads live at <https://notchbud.vercel.app/releases>.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and versions follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.7] - 2026-11-26

### Added
- Custom OpenAI-compatible endpoints alongside Groq: a three-step setup
  (base URL, API key, model ID) with key validation and model discovery,
  manual model entry, and chat-model filtering of the catalog.
- Keyless local servers work with a blank key (Ollama, LM Studio); remote
  URLs require HTTPS, HTTP stays localhost-only.
- Settings → Edit connection switches providers without dropping the
  active connection; chat history carries over.

### Fixed
- Windows-key chords (Win+V clipboard history, Win+D) are no longer eaten
  by the island's keyboard hook.
- Shift-modified characters (e.g. `"`) no longer typed as their unshifted form.
- A rejected custom key stays on the key step with the error instead of
  failing later at the first chat.

### Changed
- Auto-updates prefer the versioned installer asset.
- Saved-login storage moved to a new format; the first launch after
  updating signs you in again, once.

## [0.1.6] - 2026-11-24

### Added
- The WebView sleeps after 5 idle seconds and wakes on hover, so idle
  memory stays low.
- Replies keep running while you browse the island, with a notification
  when a hidden reply finishes.

### Fixed
- A missed reply piece when the island closes mid-stream.

## [0.1.5] - 2026-11-23

### Changed
- Version bump and a fresh installer — no app changes.

## [0.1.4] - 2026-11-23

### Added
- The app checks GitHub Releases and updates itself.
- Refreshed README, screenshots, and contributing guide.

## [0.1.3] - 2026-11-18

### Added
- First public release: a floating island on top of your screen — hover
  to open it.
- Streaming answers from Groq, billed to your own API key.
- Saved chats with auto titles, plus notifications when replies land.
- Screenshot questions about anything visible on your screen.
- Sealed key storage, a per-user installer, and release builds from CI.

[0.1.7]: https://github.com/ankitdey01/notchbud/releases/tag/v0.1.7
[0.1.6]: https://github.com/ankitdey01/notchbud/releases/tag/v0.1.6
[0.1.5]: https://github.com/ankitdey01/notchbud/releases/tag/v0.1.5
[0.1.4]: https://github.com/ankitdey01/notchbud/releases/tag/v0.1.4
[0.1.3]: https://github.com/ankitdey01/notchbud/releases/tag/v0.1.3
