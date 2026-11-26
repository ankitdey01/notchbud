// user.rs — greeting helpers for the island.
//
// An API key carries no account name, so the greeting shows the day part
// alone. This module keeps only the greeting math that formats it.

/// Local hour in 0..=23 via the OS clock (no extra deps, respects the user's
/// time zone and DST — the greeting follows "their" morning/evening).
pub fn local_hour() -> u32 {
    let t = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
    (t.wHour as u32) % 24
}

/// Pure greeting picker (injected hour -> unit-testable). Boundaries match the
/// everyday sense: 5–11 morning, 12–16 afternoon, 17–21 evening, else night.
pub fn greeting_for_hour(hour: u32, name: &str) -> String {
    let h = hour % 24;
    let part = if (5..12).contains(&h) {
        "Good Morning"
    } else if (12..17).contains(&h) {
        "Good Afternoon"
    } else if (17..22).contains(&h) {
        "Good Evening"
    } else {
        "Sleep Already"
    };
    if name.is_empty() {
        part.to_string()
    } else {
        format!("{part}, {name}")
    }
}

/// Greeting for right now: "Good Evening, Alex" at 7pm, etc. An empty name
/// greets without one ("Good Evening") so signed-in accounts without a name
/// claim still read naturally.
pub fn greeting_now(name: &str) -> String {
    greeting_for_hour(local_hour(), name)
}

const WEEKDAYS: [&str; 7] = [
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
];

const MONTHS: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

/// Pure date-time formatter (clock parts -> unit-testable).
/// "Tuesday, 18 November 2026, 21:42".
pub fn format_local_time(
    day_of_week: u16,
    day: u16,
    month: u16,
    year: u16,
    hour: u16,
    minute: u16,
) -> String {
    let weekday = WEEKDAYS.get(day_of_week as usize).unwrap_or(&"?");
    let month = MONTHS.get(month.wrapping_sub(1) as usize).unwrap_or(&"?");
    format!(
        "{weekday}, {day} {month} {year}, {:02}:{:02}",
        hour % 24,
        minute % 60
    )
}

/// Current local date-time as plain text for the model prompt.
/// Same OS clock as `local_hour` (no extra deps, honors time zone + DST).
pub fn local_now_text() -> String {
    let t = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
    format!(
        "{} local time",
        format_local_time(t.wDayOfWeek, t.wDay, t.wMonth, t.wYear, t.wHour, t.wMinute)
    )
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
    fn greeting_keeps_name_as_is() {
        // No title-casing: names pass through untouched.
        assert_eq!(
            greeting_for_hour(19, "alex kumar"),
            "Good Evening, alex kumar"
        );
        assert_eq!(greeting_for_hour(9, ""), "Good Morning");
        assert_eq!(
            greeting_for_hour(19, "José-O'Brien Jr."),
            "Good Evening, José-O'Brien Jr."
        );
        assert_eq!(greeting_for_hour(19, "McDonald"), "Good Evening, McDonald");
        assert_eq!(
            greeting_for_hour(19, "anne-marie"),
            "Good Evening, anne-marie"
        );
        assert_eq!(greeting_for_hour(19, "o'brien"), "Good Evening, o'brien");
    }

    #[test]
    fn local_time_formats_plainly() {
        assert_eq!(
            format_local_time(2, 18, 11, 2026, 21, 42),
            "Tuesday, 18 November 2026, 21:42"
        );
        assert_eq!(
            format_local_time(0, 1, 1, 2023, 5, 7),
            "Sunday, 1 January 2023, 05:07"
        );
    }
}
