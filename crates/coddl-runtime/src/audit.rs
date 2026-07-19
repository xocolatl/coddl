//! SQL query-audit log.
//!
//! Every SQL statement the runtime executes against a backend is appended to
//! a log file, one line each:
//!
//! ```text
//! YYYY-MM-DD HH:MM:SS.mmm - <backend> - <sql>
//! ```
//!
//! UTC, millisecond precision. The line carries the **expanded** SQL (bound
//! parameter values inlined), which is what a human self-audit wants — but it
//! can therefore contain PII/secrets from filter values. There is no
//! template-only (`?`) mode: the `rusqlite` `trace` callback only ever
//! delivers the expanded statement.
//!
//! This is the single observability chokepoint. Each backend installs one
//! connection-level trace hook that forwards to [`record`]; the sink (file
//! handle, timestamp, line format) lives here and is backend-agnostic, so a
//! future Postgres backend adds only a thin per-backend interception.
//!
//! Configuration, resolved once (lazily, on first `record` — so it never
//! depends on `coddl_runtime_init` having run):
//! - `CODDL_AUDIT_LOG` — path to the log file (default `./coddl-audit.log`).
//!   An empty value disables logging.
//! - `CODDL_AUDIT_LOG_DISABLE` — if set (any value), disables logging.
//!
//! Logging is observability only: any I/O error makes the sink a silent
//! no-op and never changes whether the program succeeds.

use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

const DEFAULT_PATH: &str = "coddl-audit.log";

enum Sink {
    Disabled,
    /// Connections are opened `SQLITE_OPEN_NO_MUTEX`, so SQLite does *not*
    /// serialize trace callbacks — this `Mutex` is what serializes concurrent
    /// writers to the one file handle.
    Writer(Mutex<BufWriter<File>>),
}

fn sink() -> &'static Sink {
    static SINK: OnceLock<Sink> = OnceLock::new();
    SINK.get_or_init(open_sink)
}

fn open_sink() -> Sink {
    if std::env::var_os("CODDL_AUDIT_LOG_DISABLE").is_some() {
        return Sink::Disabled;
    }
    let explicit = std::env::var_os("CODDL_AUDIT_LOG").is_some();
    // The runtime's own `cargo test` run must never drop a stray log file in
    // the crate directory; tests opt in explicitly via CODDL_AUDIT_LOG.
    if cfg!(test) && !explicit {
        return Sink::Disabled;
    }
    let path = match std::env::var("CODDL_AUDIT_LOG") {
        Ok(p) if p.is_empty() => return Sink::Disabled,
        Ok(p) => p,
        Err(_) => DEFAULT_PATH.to_string(),
    };
    match OpenOptions::new().create(true).append(true).open(&path) {
        Ok(f) => Sink::Writer(Mutex::new(BufWriter::new(f))),
        Err(_) => Sink::Disabled, // silent fallback — logging must never break the program
    }
}

/// Append one executed statement to the audit log. Backend-agnostic: each
/// backend's trace hook calls this with its own label (`"sqlite"`, later
/// `"postgres"`). No-op when logging is disabled or the file is unwritable.
pub(crate) fn record(backend: &str, sql: &str) {
    let writer = match sink() {
        Sink::Disabled => return,
        Sink::Writer(w) => w,
    };
    let dur = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let line = format_line(dur.as_secs() as i64, dur.subsec_millis(), backend, sql);
    if let Ok(mut guard) = writer.lock() {
        let _ = writeln!(guard, "{line}");
        let _ = guard.flush();
    }
}

/// Format one log line (without the trailing newline). Pure — unit-tested.
fn format_line(secs: i64, millis: u32, backend: &str, sql: &str) -> String {
    format!("{} - {} - {}", format_utc(secs, millis), backend, sql)
}

/// Render a UNIX timestamp (seconds + millis) as `YYYY-MM-DD HH:MM:SS.mmm`
/// in UTC. Pure.
fn format_utc(secs: i64, millis: u32) -> String {
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    let (hh, mm, ss) = (tod / 3600, (tod % 3600) / 60, tod % 60);
    format!("{y:04}-{m:02}-{d:02} {hh:02}:{mm:02}:{ss:02}.{millis:03}")
}

/// Howard Hinnant's days-since-1970-01-01 → (year, month, day), proleptic
/// Gregorian. Valid across the full i64 range; we only ever feed it `>= 0`.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_formats_to_unix_zero() {
        assert_eq!(format_utc(0, 0), "1970-01-01 00:00:00.000");
    }

    #[test]
    fn millis_are_zero_padded_to_three() {
        assert_eq!(format_utc(0, 7), "1970-01-01 00:00:00.007");
        assert_eq!(format_utc(0, 70), "1970-01-01 00:00:00.070");
        assert_eq!(format_utc(0, 700), "1970-01-01 00:00:00.700");
    }

    #[test]
    fn leap_day_2020_02_29() {
        // 1_582_934_400 == 2020-02-29 00:00:00 UTC — guards the hand-rolled
        // civil-date helper against leap-year off-by-ones.
        assert_eq!(format_utc(1_582_934_400, 0), "2020-02-29 00:00:00.000");
    }

    #[test]
    fn known_datetime_with_time_of_day() {
        // 1_609_459_200 == 2021-01-01 00:00:00 UTC; add 13:45:07.221.
        let secs = 1_609_459_200 + 13 * 3600 + 45 * 60 + 7;
        assert_eq!(format_utc(secs, 221), "2021-01-01 13:45:07.221");
    }

    #[test]
    fn line_matches_the_spec_format() {
        let line = format_line(
            1_609_459_200,
            0,
            "sqlite",
            "SELECT message FROM greetings WHERE id = 1",
        );
        assert_eq!(
            line,
            "2021-01-01 00:00:00.000 - sqlite - SELECT message FROM greetings WHERE id = 1"
        );
    }
}
