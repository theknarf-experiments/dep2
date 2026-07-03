//! Clock streaming plugin: a heartbeat that gives rules a notion of *now*.
//!
//! ```text
//! now(iso, epoch)
//! ```
//!
//! One row: the current UTC time as an ISO-8601 string and as Unix epoch
//! seconds. Every `tick` seconds (default 60) the row is retracted and
//! re-inserted with the new time, so anything derived from `now` — "deadlines
//! within 7 days", "stale entries" — is incrementally re-evaluated on
//! the heartbeat. The epoch is rounded down to a multiple of `tick`, so
//! restarts within the same tick produce the identical row.
//!
//! Pairs with the `date_epoch(s)` builtin, which turns ISO date columns into
//! epoch seconds: `date_epoch(End) < NowE + 604800`.
//!
//! Config: `tick` (seconds, default 60), or `fixed` (an ISO-8601 instant —
//! the clock never advances; for tests and reproducible runs).

use std::collections::HashMap;
use std::time::{Duration, Instant, SystemTime};

use dep2_plugin::reconcile::{multiset, reconcile, CanonValue, Multiset};
use dep2_plugin::{
    ColumnDef, DataSchema, DataType, Plugin, PluginContext, Source, StreamOutput,
    StreamingDataProvider, StreamingDataSource, ValueSink,
};

const RELATION: &str = "now";
const UNIT: &str = "clock";
const DEFAULT_TICK_SECS: u64 = 60;

const KNOWN_KEYS: &[&str] = &["tick", "fixed"];

pub struct ClockPlugin;

impl Plugin for ClockPlugin {
    fn name(&self) -> &str {
        "clock"
    }

    fn setup(&self, ctx: &mut PluginContext) {
        ctx.register(self.name());
        ctx.register_streaming_data_provider(Box::new(ClockProvider));
    }
}

struct ClockProvider;

impl StreamingDataProvider for ClockProvider {
    fn name(&self) -> &str {
        "clock"
    }

    fn open_stream(
        &self,
        config: &HashMap<String, String>,
    ) -> Result<Box<dyn StreamingDataSource>, String> {
        for key in config.keys() {
            if !KNOWN_KEYS.contains(&key.as_str()) {
                return Err(format!(
                    "clock: unknown config attribute '{}' (known: {})",
                    key,
                    KNOWN_KEYS.join(", ")
                ));
            }
        }
        let tick_secs: u64 =
            match config.get("tick") {
                Some(v) => v.parse().ok().filter(|&t| t > 0).ok_or_else(|| {
                    format!("clock: 'tick' must be a positive integer, got '{}'", v)
                })?,
                None => DEFAULT_TICK_SECS,
            };
        let fixed =
            match config.get("fixed") {
                Some(iso) => Some(parse_iso_epoch(iso).ok_or_else(|| {
                    format!("clock: 'fixed' is not an ISO-8601 instant: '{}'", iso)
                })?),
                None => None,
            };
        Ok(Box::new(ClockSource {
            tick: Duration::from_secs(tick_secs),
            tick_secs: tick_secs as i64,
            fixed,
        }))
    }
}

#[derive(Clone)]
struct ClockSource {
    tick: Duration,
    tick_secs: i64,
    /// Fixed epoch seconds (the clock never advances) — tests / reproducibility.
    fixed: Option<i64>,
}

impl StreamingDataSource for ClockSource {
    fn outputs(&self) -> Vec<StreamOutput> {
        vec![StreamOutput {
            relation: RELATION.to_string(),
            schema: DataSchema {
                columns: [("iso", DataType::String), ("epoch", DataType::Integer)]
                    .into_iter()
                    .map(|(name, dt)| ColumnDef {
                        name: name.to_string(),
                        data_type: dt,
                    })
                    .collect(),
            },
        }]
    }

    fn seed_units(&self) -> Vec<String> {
        vec![UNIT.to_string()]
    }

    fn open(&self) -> Box<dyn Source> {
        Box::new(ClockWorker {
            source: self.clone(),
            current: Multiset::new(),
            last_ingest: None,
        })
    }
}

struct ClockWorker {
    source: ClockSource,
    current: Multiset,
    last_ingest: Option<Instant>,
}

impl ClockWorker {
    fn epoch_now(&self) -> i64 {
        match self.source.fixed {
            Some(fixed) => fixed,
            None => {
                let secs = SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0);
                // Round down to the tick so the row is stable within one tick.
                secs - secs % self.source.tick_secs
            }
        }
    }
}

impl Source for ClockWorker {
    fn ingest(&mut self, _unit: &str, sink: &mut dyn ValueSink) {
        let epoch = self.epoch_now();
        let new = multiset([vec![
            CanonValue::str(format_iso(epoch)),
            CanonValue::Int(epoch),
        ]]);
        reconcile(sink, RELATION, &self.current, &new);
        self.current = new;
        self.last_ingest = Some(Instant::now());
    }

    fn poll_changes(&mut self) -> Vec<String> {
        if self.source.fixed.is_some() {
            return Vec::new(); // a fixed clock never ticks
        }
        let due = self
            .last_ingest
            .map(|t| t.elapsed() >= self.source.tick)
            .unwrap_or(false);
        if due {
            vec![UNIT.to_string()]
        } else {
            Vec::new()
        }
    }
}

// ---------------------------------------------------------------------------
// Civil-calendar conversion (Howard Hinnant's algorithms — no calendar tables).
// The engine-side `date_epoch` builtin uses the same day mapping, so a `now`
// row and a parsed date column always agree.
// ---------------------------------------------------------------------------

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Epoch seconds -> `YYYY-MM-DDTHH:MM:SSZ`.
fn format_iso(epoch: i64) -> String {
    let days = epoch.div_euclid(86400);
    let rem = epoch.rem_euclid(86400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        y,
        m,
        d,
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// `YYYY-MM-DD[THH:MM[:SS]][Z]` -> epoch seconds (date-only means midnight).
fn parse_iso_epoch(s: &str) -> Option<i64> {
    let (date, time) = match s.split_once('T') {
        Some((date, time)) => (date, Some(time.trim_end_matches('Z'))),
        None => (s.trim_end_matches('Z'), None),
    };
    let mut parts = date.split('-');
    let y: i64 = parts.next()?.parse().ok()?;
    let m: i64 = parts.next()?.parse().ok()?;
    let d: i64 = parts.next()?.parse().ok()?;
    if parts.next().is_some() || !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let (h, min, sec) = match time {
        None => (0, 0, 0),
        Some(time) => {
            let mut parts = time.split(':');
            let h: i64 = parts.next()?.parse().ok()?;
            let min: i64 = parts.next()?.parse().ok()?;
            let sec: i64 = match parts.next() {
                Some(sec) => sec.parse().ok()?,
                None => 0,
            };
            (h, min, sec)
        }
    };
    Some(days_from_civil(y, m, d) * 86400 + h * 3600 + min * 60 + sec)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Recorder(Vec<(String, Vec<dep2_plugin::DataValue>, isize)>);

    impl ValueSink for Recorder {
        fn push(&mut self, relation: &str, row: &[dep2_plugin::DataValue], diff: isize) {
            self.0.push((relation.to_string(), row.to_vec(), diff));
        }
    }

    #[test]
    fn iso_round_trips() {
        for iso in [
            "1970-01-01T00:00:00Z",
            "2026-07-02T23:59:59Z",
            "2000-02-29T12:00:00Z", // leap day
            "1969-12-31T23:00:00Z", // pre-epoch
        ] {
            let epoch = parse_iso_epoch(iso).unwrap();
            assert_eq!(format_iso(epoch), iso);
        }
        assert_eq!(parse_iso_epoch("1970-01-02"), Some(86400));
        assert_eq!(parse_iso_epoch("nope"), None);
    }

    #[test]
    fn fixed_clock_emits_once_and_never_ticks() {
        let provider = ClockProvider;
        let mut config = HashMap::new();
        config.insert("fixed".to_string(), "2026-07-02T00:00:00Z".to_string());
        let source = provider.open_stream(&config).unwrap();
        let mut worker = source.open();

        let mut sink = Recorder(Vec::new());
        worker.ingest(UNIT, &mut sink);
        assert_eq!(sink.0.len(), 1);
        let (rel, row, diff) = &sink.0[0];
        assert_eq!(rel, "now");
        assert_eq!(*diff, 1);
        assert_eq!(
            row[0],
            dep2_plugin::DataValue::Str("2026-07-02T00:00:00Z".into())
        );
        assert_eq!(
            row[1],
            dep2_plugin::DataValue::Integer(parse_iso_epoch("2026-07-02").unwrap())
        );

        // Re-ingest is a no-op; a fixed clock never reports changes.
        let mut sink = Recorder(Vec::new());
        worker.ingest(UNIT, &mut sink);
        assert!(sink.0.is_empty());
        assert!(worker.poll_changes().is_empty());
    }

    #[test]
    fn live_clock_rounds_to_tick_and_reticks() {
        let provider = ClockProvider;
        let mut config = HashMap::new();
        config.insert("tick".to_string(), "60".to_string());
        let source = provider.open_stream(&config).unwrap();
        let mut worker = source.open();

        let mut sink = Recorder(Vec::new());
        worker.ingest(UNIT, &mut sink);
        assert_eq!(sink.0.len(), 1);
        let dep2_plugin::DataValue::Integer(epoch) = sink.0[0].1[1] else {
            panic!("expected integer epoch");
        };
        assert_eq!(epoch % 60, 0, "epoch rounds down to the tick");
        // Just ingested: not due yet.
        assert!(worker.poll_changes().is_empty());
    }

    #[test]
    fn provider_validates_config() {
        let provider = ClockProvider;
        let mut config = HashMap::new();
        config.insert("tick".to_string(), "0".to_string());
        assert!(provider.open_stream(&config).is_err());
        let mut config = HashMap::new();
        config.insert("fixed".to_string(), "not a date".to_string());
        assert!(provider.open_stream(&config).is_err());
    }
}
