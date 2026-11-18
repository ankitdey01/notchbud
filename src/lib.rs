// notchbud library crate: motion math (state) + shell renderer (render) +
// WebView2 island content (island_web) + the Groq connector (connector)
// + greeting helpers (user) + local chat sessions (sessions). Window shell
// (main.rs), tray and prefs live in the binary crate.
pub mod connector;
pub mod island_web;
pub mod render;
pub mod sessions;
pub mod state;
pub mod user;

/// Local profile dir: %APPDATA%\notchbud by default. Tests point
/// NOTCHBUD_DATA_DIR at a temp dir so they never touch the real profile.
pub fn data_dir() -> Option<std::path::PathBuf> {
    if let Some(dir) = std::env::var_os("NOTCHBUD_DATA_DIR") {
        if !dir.is_empty() {
            return Some(std::path::PathBuf::from(dir));
        }
    }
    std::env::var_os("APPDATA").map(|base| std::path::Path::new(&base).join("notchbud"))
}
