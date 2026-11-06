// quota_spike — probe what the signed-in ChatGPT plan allows over the
// documented SIWC flow: browser authorize once, then print the account's
// model catalog and one streamed answer through the public Responses API.
//
//     cargo run --example quota_spike
//
// Run it, approve in the opened browser, wait for the callback.

use notchbud::connector::{auth, chat, chat::Msg, chat::Role};

fn main() {
    let host_id = match auth::load_or_create_host_id() {
        Ok(h) => h,
        Err(e) => {
            eprintln!("sign-in could not start: {e}");
            return;
        }
    };
    let saved = auth::load_tokens();
    let (pending, url) = match auth::begin_authorize(saved.as_ref(), &host_id) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("sign-in could not start: {e}");
            return;
        }
    };
    println!("Open this URL in your browser:\n\n    {url}\n");
    // Also try to open it directly.
    let _ = open::that(&url);
    let (cb, meta) = match auth::wait_callback(pending) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("sign-in failed: {e}");
            return;
        }
    };
    let data = match auth::exchange_code(&meta, &cb.code, &cb.client_id) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("sign-in failed: {e}");
            return;
        }
    };
    let tokens = match auth::finish_signin(&data, &cb.client_id, &meta.nonce, &host_id, saved.as_ref()) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("sign-in failed: {e}");
            return;
        }
    };
    if let Err(e) = auth::save_tokens(&tokens) {
        eprintln!("could not save credentials ({e}) — sign-in works, persistence does not");
        std::process::exit(2);
    }
    println!("access token expires at: {} (epoch s)", tokens.expires_at);
    println!("subject: {}", tokens.subject);
    println!("plan usage allowed: {}", tokens.plan_allowed);

    // model inventory: what this account's catalog offers
    match auth::list_models(&tokens) {
        Ok(models) if !models.is_empty() => {
            let names: Vec<String> =
                models.iter().map(|m| format!("{} ({})", m.slug, m.display_name)).collect();
            println!("models: {}", names.join(", "));
        }
        _ => println!("models: listing unreadable"),
    }

    if !tokens.plan_allowed {
        println!("plan usage not granted — inference disabled by design.");
        return;
    }

    // one streamed request against the first catalog model
    let model = match auth::pick_model(&tokens) {
        Ok(m) => m,
        Err(m) => {
            println!("request: FAILED — {m}");
            return;
        }
    };
    println!("trying model {model} …");
    let history = [Msg { role: Role::User, text: "Reply with the single word: OK".into() }];
    let res = chat::stream_chat(&tokens, &model, &history, None, |ev| match ev {
        chat::StreamEvent::Delta(d) => {
            print!("{d}");
            true
        }
        _ => true,
    });
    println!();
    match res {
        Ok(()) => println!("request: OK (streamed above)"),
        Err(m) => println!("request: FAILED — {m}"),
    }
    println!("\nRecord these numbers before any notchbud code claims a tier.");
}

mod open {
    pub fn that(url: &str) -> std::io::Result<()> {
        use std::os::windows::process::CommandExt;
        // DETACHED_PROCESS so no console window flashes.
        const DETACHED_PROCESS: u32 = 0x00000008;
        std::process::Command::new("rundll32")
            .arg("url.dll,FileProtocolHandler")
            .arg(url)
            .creation_flags(DETACHED_PROCESS)
            .spawn()
            .map(|_| ())
    }
}
