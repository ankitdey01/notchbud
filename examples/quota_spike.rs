// quota_spike — PLAN.md Milestone 0: run the device flow once against a
// free/Go account, then one Codex request, and print what the plan allows:
// HTTP status, the returned model list, the x-codex-* quota headers, and one
// streamed answer. The recorded numbers decide dyland's whole positioning.
//
//     cargo run --example quota_spike
//
// Run it, open the printed URL in a browser, enter the code, wait.

use dyland::connector::{auth, chat, chat::Msg, chat::Role};

fn main() {
    let login = match auth::start_device_login() {
        Ok(l) => l,
        Err(e) => {
            eprintln!("sign-in could not start: {e}");
            return;
        }
    };
    println!("Open {} and enter this code:\n", auth::VERIFICATION_URL);
    println!("    {}\n", login.user_code);

    let tokens = loop {
        std::thread::sleep(std::time::Duration::from_millis(login.interval_ms));
        match auth::poll_device_login(&login) {
            Ok(auth::Poll::Pending) => {
                print!(".");
                continue;
            }
            Ok(auth::Poll::Connected(t)) => break t,
            Err(e) => {
                println!();
                eprintln!("sign-in failed: {e}");
                return;
            }
        }
    };
    // save for the app: dyland.exe picks these up from %APPDATA%\dyland\auth.bin
    let _ = auth::save_tokens(&tokens);
    println!();
    println!("access token expires at: {} (epoch s)", tokens.expires_at);
    println!("account id: {}", tokens.account_id.as_deref().unwrap_or("(none)"));

    // model inventory: what this plan's listing offers
    match auth::list_models(&tokens) {
        Ok(models) if !models.is_empty() => println!("models: {}", models.join(", ")),
        _ => println!("models: listing unreadable — defaults: {}", auth::FALLBACK_MODELS.join(", ")),
    }

    // one streamed request against the default model
    let model = auth::pick_model(&tokens);
    println!("trying model {model} …");
    let history = [Msg { role: Role::User, text: "Reply with the single word: OK".into() }];
    let res = chat::stream_chat(&tokens, &model, &history, |ev| match ev {
        chat::StreamEvent::Delta(d) => {
            print!("{d}");
            true
        }
        _ => true,
    });
    println!();
    // also print the raw response headers once: they carry the quota facts
    // (x-codex-plan-type, x-codex-active-limit, …) on the plain POST path.
    print_quota_headers(&tokens, &model, &history);
    match res {
        Ok(()) => println!("request: OK (streamed above)"),
        Err(m) => println!("request: FAILED — {m}"),
    }
    println!("\nRecord these numbers before any dyland code claims a tier.");
}

fn print_quota_headers(tokens: &auth::Tokens, model: &str, history: &[Msg]) {
    use ureq::AgentBuilder;
    let agent = AgentBuilder::new()
        .user_agent(auth::USER_AGENT)
        .build();
    let mut req = agent
        .post(chat::RESPONSES_URL)
        .set("authorization", &format!("Bearer {}", tokens.access))
        .set("originator", auth::ORIGINATOR)
        .set("accept", "text/event-stream")
        .set("content-type", "application/json");
    if let Some(account) = &tokens.account_id {
        req = req.set("chatgpt-account-id", account);
    }
    match req.send_string(&chat::responses_body(model, history).to_string()) {
        Ok(r) => {
            println!("status: {}", r.status());
            for name in r.headers_names() {
                if name.starts_with("x-codex") {
                    println!("  {name}: {}", r.header(&name).unwrap_or(""));
                }
            }
        }
        Err(e) => println!("status: error {e}"),
    }
}
