// Google Calendar through its secret iCal address — no OAuth, no Google Cloud
// project: the user pastes the "Dirección secreta en formato iCal" once and it
// lives in the Credential Manager like every other key.
//
// The feed can hold years of events. Only what can still matter is sent to the
// island: single events starting within a couple of days, and recurring series
// that haven't ended. Expanding the recurrences, and the time zones that come
// with them, happens in the island (src/island/calendar.ts), where Intl knows
// every IANA zone — no time-zone database to ship on this side.

use serde_json::{json, Value};

/// How many events at most cross over to the island.
const MAX_EVENTS: usize = 400;
/// Single events are kept from this many days before today to this many after,
/// compared on raw `YYYYMMDD` dates; the margin covers any time-zone offset.
const DAYS_BEHIND: i64 = 2;
const DAYS_AHEAD: i64 = 3;

/// The calendar's own lines, unfolded: a line starting with a space or a tab
/// continues the one before it (RFC 5545 §3.1).
fn unfold(text: &str) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for raw in text.split('\n') {
        let raw = raw.strip_suffix('\r').unwrap_or(raw);
        if let Some(rest) = raw.strip_prefix(' ').or_else(|| raw.strip_prefix('\t')) {
            if let Some(last) = lines.last_mut() {
                last.push_str(rest);
                continue;
            }
        }
        lines.push(raw.to_string());
    }
    lines
}

/// `DTSTART;TZID=America/Mexico_City:20261001T100000` → name, params, value.
fn split_property(line: &str) -> Option<(String, Vec<(String, String)>, String)> {
    // The value starts at the first colon outside a quoted parameter.
    let mut quoted = false;
    let colon = line.char_indices().find(|&(_, c)| {
        if c == '"' {
            quoted = !quoted;
        }
        c == ':' && !quoted
    })?.0;
    let (head, value) = (&line[..colon], &line[colon + 1..]);
    let mut parts = head.split(';');
    let name = parts.next()?.to_ascii_uppercase();
    let params = parts
        .filter_map(|p| {
            let (k, v) = p.split_once('=')?;
            Some((k.to_ascii_uppercase(), v.trim_matches('"').to_string()))
        })
        .collect();
    Some((name, params, value.to_string()))
}

/// TEXT values escape `\\`, `\;`, `\,` and `\n`.
fn unescape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') | Some('N') => out.push('\n'),
            Some(other) => out.push(other),
            None => {}
        }
    }
    out
}

fn param<'a>(params: &'a [(String, String)], key: &str) -> Option<&'a str> {
    params.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

/// A date-time as the island needs it: the raw value, its zone, all-day or not.
fn when(params: &[(String, String)], value: &str) -> Value {
    json!({
        "value": value.trim(),
        "tzid": param(params, "TZID"),
        "date": param(params, "VALUE") == Some("DATE") || value.trim().len() == 8,
    })
}

/// The first video-call link in the event: Meet, Zoom or Teams.
fn meeting_link(texts: &[&str]) -> Option<String> {
    const HOSTS: [&str; 4] = [
        "https://meet.google.com/",
        "https://zoom.us/",
        "https://teams.microsoft.com/",
        "https://us02web.zoom.us/",
    ];
    for text in texts {
        for host in HOSTS {
            if let Some(at) = text.find(host) {
                let link: String = text[at..]
                    .chars()
                    .take_while(|c| !c.is_whitespace() && !matches!(c, '"' | '<' | '>' | ')' | '\\'))
                    .collect();
                return Some(link);
            }
        }
    }
    None
}

/// Days since 1970-01-01 → (year, month, day). Howard Hinnant's civil_from_days.
fn civil(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

fn yyyymmdd(days: i64) -> String {
    let (y, m, d) = civil(days);
    format!("{y:04}{m:02}{d:02}")
}

/// Today's UTC day number.
fn today() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64 / 86_400)
        .unwrap_or(0)
}

/// The events worth sending, from the whole feed. `today` is a UTC day number.
pub fn parse(text: &str, today_utc: Option<i64>) -> Vec<Value> {
    let day = today_utc.unwrap_or_else(today);
    let from = yyyymmdd(day - DAYS_BEHIND);
    let to = yyyymmdd(day + DAYS_AHEAD);

    let mut events = Vec::new();
    let mut current: Option<Vec<(String, Vec<(String, String)>, String)>> = None;
    for line in unfold(text) {
        match line.trim() {
            "BEGIN:VEVENT" => {
                current = Some(Vec::new());
                continue;
            }
            "END:VEVENT" => {
                if let Some(props) = current.take() {
                    if let Some(event) = event(&props, &from, &to) {
                        events.push(event);
                        if events.len() >= MAX_EVENTS {
                            break;
                        }
                    }
                }
                continue;
            }
            _ => {}
        }
        if let Some(props) = current.as_mut() {
            if let Some(prop) = split_property(&line) {
                props.push(prop);
            }
        }
    }
    events
}

fn event(props: &[(String, Vec<(String, String)>, String)], from: &str, to: &str) -> Option<Value> {
    let get = |name: &str| props.iter().find(|(n, _, _)| n == name);
    let (_, start_params, start) = get("DTSTART")?;
    let rrule = get("RRULE").map(|(_, _, v)| v.clone());

    if let Some(rule) = &rrule {
        // A series whose UNTIL is behind us is over.
        let until = rule
            .split(';')
            .find_map(|part| part.strip_prefix("UNTIL="))
            .map(|u| u.get(..8).unwrap_or(u).to_string());
        if until.is_some_and(|u| u.as_str() < from) {
            return None;
        }
    } else {
        let date = start.get(..8)?;
        if date < from || date > to {
            return None;
        }
    }

    let text = |name: &str| get(name).map(|(_, _, v)| unescape(v)).unwrap_or_default();
    let summary = text("SUMMARY");
    let description = text("DESCRIPTION");
    let location = text("LOCATION");
    let conference = text("X-GOOGLE-CONFERENCE");
    let exdates: Vec<Value> = props
        .iter()
        .filter(|(n, _, _)| n == "EXDATE")
        .flat_map(|(_, params, v)| v.split(',').map(move |one| when(params, one)))
        .collect();

    Some(json!({
        "uid": text("UID"),
        "summary": if summary.is_empty() { "Reunión".to_string() } else { summary },
        "start": when(start_params, start),
        "end": get("DTEND").map(|(_, p, v)| when(p, v)),
        "duration": get("DURATION").map(|(_, _, v)| v.clone()),
        "rrule": rrule,
        "exdates": exdates,
        "recurrenceId": get("RECURRENCE-ID").map(|(_, p, v)| when(p, v)),
        "cancelled": text("STATUS").eq_ignore_ascii_case("CANCELLED"),
        "link": meeting_link(&[&conference, &location, &description]),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FEED: &str = "BEGIN:VCALENDAR\r\n\
BEGIN:VEVENT\r\n\
DTSTART;TZID=America/Mexico_City:20261001T100000\r\n\
DTEND;TZID=America/Mexico_City:20261001T103000\r\n\
RRULE:FREQ=WEEKLY;BYDAY=MO,TH\r\n\
EXDATE;TZID=America/Mexico_City:20261008T100000\r\n\
SUMMARY:Daily\\, equipo\r\n\
DESCRIPTION:Únete: https://meet.google.com/abc-defg-hij\\nGracias\r\n\
UID:daily@google.com\r\n\
END:VEVENT\r\n\
BEGIN:VEVENT\r\n\
DTSTART:20261002T170000Z\r\n\
DTEND:20261002T180000Z\r\n\
SUMMARY:Revisión con un nombre muy lar\r\n go que se dobla\r\n\
UID:one@google.com\r\n\
END:VEVENT\r\n\
BEGIN:VEVENT\r\n\
DTSTART:20200101T170000Z\r\n\
SUMMARY:Viejo\r\n\
UID:old@google.com\r\n\
END:VEVENT\r\n\
BEGIN:VEVENT\r\n\
DTSTART;VALUE=DATE:20200101\r\n\
RRULE:FREQ=YEARLY;UNTIL=20210101\r\n\
SUMMARY:Serie terminada\r\n\
UID:ended@google.com\r\n\
END:VEVENT\r\n\
END:VCALENDAR\r\n";

    /// 2026-10-01 as a UTC day number.
    const OCT_1: i64 = 20_727;

    #[test]
    fn day_numbers_are_civil_dates() {
        assert_eq!(yyyymmdd(0), "19700101");
        assert_eq!(yyyymmdd(OCT_1), "20261001");
    }

    #[test]
    fn keeps_only_what_can_still_matter() {
        let events = parse(FEED, Some(OCT_1));
        let names: Vec<&str> = events.iter().map(|e| e["summary"].as_str().unwrap()).collect();
        assert_eq!(names, ["Daily, equipo", "Revisión con un nombre muy largo que se dobla"]);
        let daily = &events[0];
        assert_eq!(daily["start"]["tzid"], "America/Mexico_City");
        assert_eq!(daily["start"]["value"], "20261001T100000");
        assert_eq!(daily["rrule"], "FREQ=WEEKLY;BYDAY=MO,TH");
        assert_eq!(daily["exdates"][0]["value"], "20261008T100000");
        assert_eq!(daily["link"], "https://meet.google.com/abc-defg-hij");
        assert_eq!(events[1]["start"]["tzid"], Value::Null);
    }
}
