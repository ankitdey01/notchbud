// user.rs — greeting helpers for the island.
//
// The name now comes from the signed-in OpenAI account (the ID-token `name`
// claim, stored on `Tokens`), never typed by hand and never stored in a
// separate file. This module keeps only the greeting math that formats it.

/// Canonical display width for the greeting line: account names are
/// truncated to this many characters so they fit inside the 420u island.
pub const NAME_MAX_CHARS: usize = 30;

/// Local hour in 0..=23 via the OS clock (no extra deps, respects the user's
/// time zone and DST — the greeting follows "their" morning/evening).
pub fn local_hour() -> u32 {
    let t = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
    (t.wHour as u32) % 24
}

/// Title-case for display: first letter of every word in caps, everything
/// else untouched — so "José-O'Brien Jr." and "McDonald" survive intact
/// while "alex kumar" greets as "Alex Kumar". Words start after whitespace,
/// hyphens, and apostrophes ("anne-marie" -> "Anne-Marie").
pub fn title_case_words(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut word_start = true;
    for ch in s.chars() {
        if ch.is_whitespace() {
            word_start = true;
            out.push(ch);
        } else if word_start && ch.is_alphabetic() {
            // Uppercase the head only (some lowercases expand, e.g. ß->SS).
            let mut up = ch.to_uppercase();
            out.push(up.next().unwrap_or(ch));
            out.extend(up);
            word_start = false;
        } else {
            if ch == '-' || ch == '\'' || ch == '’' {
                word_start = true;
            } else {
                word_start = false;
            }
            out.push(ch);
        }
    }
    out
}

/// Pure greeting picker (injected hour -> unit-testable). Boundaries match the
/// everyday sense: 5–11 morning, 12–16 afternoon, 17–21 evening, else night.
/// Displayed title-cased: "Good Evening, Alex Kumar".
pub fn greeting_for_hour(hour: u32, name: &str) -> String {
    let h = hour % 24;
    let part = if (5..12).contains(&h) {
        "Good morning"
    } else if (12..17).contains(&h) {
        "Good afternoon"
    } else if (17..22).contains(&h) {
        "Good evening"
    } else {
        "sleep already"
    };
    if name.is_empty() {
        title_case_words(part)
    } else {
        title_case_words(&format!("{part}, {name}"))
    }
}

/// Greeting for right now: "Good Evening, Alex" at 7pm, etc. An empty name
/// greets without one ("Good Evening") so signed-in accounts without a name
/// claim still read naturally.
pub fn greeting_now(name: &str) -> String {
    greeting_for_hour(local_hour(), name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn greeting_boundaries() {
        assert_eq!(greeting_for_hour(4, "Alex"), "Sleep Already, Alex");
        assert_eq!(greeting_for_hour(5, "Alex"), "Good Morning, Alex");
        assert_eq!(greeting_for_hour(11, "Alex"), "Good Morning, Alex");
        assert_eq!(greeting_for_hour(12, "Alex"), "Good Afternoon, Alex");
        assert_eq!(greeting_for_hour(16, "Alex"), "Good Afternoon, Alex");
        assert_eq!(greeting_for_hour(17, "Alex"), "Good Evening, Alex");
        assert_eq!(greeting_for_hour(19, "Alex"), "Good Evening, Alex");
        assert_eq!(greeting_for_hour(21, "Alex"), "Good Evening, Alex");
        assert_eq!(greeting_for_hour(22, "Alex"), "Sleep Already, Alex");
        assert_eq!(greeting_for_hour(0, "Alex"), "Sleep Already, Alex");
    }

    #[test]
    fn greeting_title_cases_every_word() {
        // Lowercase names greet capped; the day part too.
        assert_eq!(greeting_for_hour(19, "alex kumar"), "Good Evening, Alex Kumar");
        assert_eq!(greeting_for_hour(9, ""), "Good Morning");
        // Already-capped names pass through untouched — never lowercased.
        assert_eq!(
            greeting_for_hour(19, "José-O'Brien Jr."),
            "Good Evening, José-O'Brien Jr."
        );
        assert_eq!(greeting_for_hour(19, "McDonald"), "Good Evening, McDonald");
        assert_eq!(greeting_for_hour(19, "anne-marie"), "Good Evening, Anne-Marie");
        assert_eq!(greeting_for_hour(19, "o'brien"), "Good Evening, O'Brien");
    }
}
