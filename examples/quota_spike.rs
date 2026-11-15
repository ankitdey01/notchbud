// quota_spike — probe what the saved Groq API key allows: print the
// model catalog and one streamed answer through Groq's OpenAI-compatible
// chat-completions API.
//
//     $env:GROQ_API_KEY = "gsk_..."
//     cargo run --example quota_spike
//
// The key may also come from the island's saved DPAPI blob.

use notchbud::connector::{auth, chat, chat::Msg, chat::Role};

fn api_key() -> String {
    if let Ok(k) = std::env::var("GROQ_API_KEY") {
        if !k.trim().is_empty() {
            return k;
        }
    }
    if let Some(saved) = auth::load_api_key() {
        return saved.key;
    }
    eprintln!("set GROQ_API_KEY or save a key in the island first");
    std::process::exit(2);
}

fn main() {
    let key = api_key();
    if auth::ApiKey::new(&key).is_none() {
        eprintln!("key doesn't look like a Groq API key (it starts with gsk_)");
        std::process::exit(2);
    }

    // model inventory: what this key's catalog offers
    match auth::list_models(&key) {
        Ok(models) if !models.is_empty() => {
            let names: Vec<String> =
                models.iter().map(|m| m.slug.clone()).collect();
            println!("models: {}", names.join(", "));
        }
        _ => println!("models: listing unreadable"),
    }

    // one streamed request against the first catalog model
    let model = match auth::pick_model(&key) {
        Ok(m) => m,
        Err(m) => {
            println!("request: FAILED — {m}");
            return;
        }
    };
    println!("trying model {model} …");
    let history = [Msg { role: Role::User, text: "Reply with the single word: OK".into() }];
    let res = chat::stream_chat(&key, &model, &history, |ev| match ev {
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
}
