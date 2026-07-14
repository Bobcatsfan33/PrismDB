//! TSV: the S0 wire format.
//!
//! Deliberately dull. The real ingestion surface is OTLP + Kafka with the OTel
//! GenAI semantic conventions (S2); until then a tab-separated file is enough to
//! get events into the engine, and it keeps the S0 slice honest about what is
//! actually built.

use prism_types::error::{PrismError, Result};
use prism_types::event::Event;

pub const HEADER: &str = "event_id\ttenant_id\tevent_time\tevent_name\tcost\terror\tbody";

fn escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('\t', "\\t")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('t') => out.push('\t'),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('\\') => out.push('\\'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

pub fn write(events: &[Event]) -> String {
    let mut s = String::from(HEADER);
    s.push('\n');
    for e in events {
        s.push_str(&format!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
            escape(&e.event_id),
            escape(&e.tenant_id),
            e.event_time,
            escape(&e.event_name),
            e.cost,
            if e.error { "1" } else { "0" },
            escape(&e.body),
        ));
    }
    s
}

/// Parse TSV. A malformed line is an error with its line number — it is not
/// skipped, because silently skipping input is how you lose data and never find
/// out.
pub fn parse(text: &str) -> Result<Vec<Event>> {
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        if i == 0 && line.starts_with("event_id\t") {
            continue;
        }
        let f: Vec<&str> = line.split('\t').collect();
        if f.len() != 7 {
            return Err(PrismError::Invalid(format!(
                "line {}: expected 7 tab-separated fields, found {}",
                i + 1,
                f.len()
            )));
        }
        out.push(Event {
            event_id: unescape(f[0]),
            tenant_id: unescape(f[1]),
            event_time: f[2].parse().map_err(|_| {
                PrismError::Invalid(format!(
                    "line {}: event_time `{}` is not an integer",
                    i + 1,
                    f[2]
                ))
            })?,
            event_name: unescape(f[3]),
            cost: f[4].parse().map_err(|_| {
                PrismError::Invalid(format!("line {}: cost `{}` is not a number", i + 1, f[4]))
            })?,
            error: match f[5] {
                "1" | "true" => true,
                "0" | "false" => false,
                other => {
                    return Err(PrismError::Invalid(format!(
                        "line {}: error `{other}` is not 0/1/true/false",
                        i + 1
                    )))
                }
            },
            body: unescape(f[6]),
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_including_tabs_and_newlines_in_the_body() {
        let events = vec![Event {
            event_id: "e1".into(),
            tenant_id: "t1".into(),
            event_time: 123,
            event_name: "llm.call".into(),
            cost: 0.25,
            error: true,
            body: "line one\twith a tab\nand a newline".into(),
        }];
        let parsed = parse(&write(&events)).unwrap();
        assert_eq!(parsed, events);
    }

    #[test]
    fn a_malformed_line_is_an_error_not_a_silent_skip() {
        let bad = format!("{HEADER}\nonly\tthree\tfields\n");
        assert!(parse(&bad).is_err());

        let bad_time = format!("{HEADER}\ne1\tt1\tnot-a-time\tn\t0.1\t0\tbody\n");
        assert!(parse(&bad_time).is_err());

        let bad_err = format!("{HEADER}\ne1\tt1\t1\tn\t0.1\tmaybe\tbody\n");
        assert!(parse(&bad_err).is_err());
    }

    #[test]
    fn header_is_optional_but_tolerated() {
        let no_header = "e1\tt1\t1\tn\t0.1\t0\tbody\n";
        assert_eq!(parse(no_header).unwrap().len(), 1);
    }
}
