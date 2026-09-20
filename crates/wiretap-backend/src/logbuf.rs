//! A bounded ring of recent log records, filled by a `tracing` layer and read
//! by `/v1/admin/logs`. It exists so an operator can see what the gateway is
//! logging without a shell on the host — `docker logs` remains the durable
//! record; this is lost on restart.

use std::collections::VecDeque;
use std::fmt::Write;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use serde::{Serialize, Serializer};
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::Context;
use tracing_subscriber::Layer;

#[derive(Debug, Clone, Serialize)]
pub struct LogRecord {
    /// Monotonic, assigned under the lock: a stable identity for the UI.
    pub seq: u64,
    pub ts: DateTime<Utc>,
    #[serde(serialize_with = "level_name")]
    pub level: Level,
    pub target: &'static str,
    pub message: String,
    /// `database=x elapsed_ms=n` — the structured fields, kept apart so the UI
    /// can dim them.
    pub fields: String,
}

fn level_name<S: Serializer>(level: &Level, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(level.as_str())
}

struct Ring {
    records: VecDeque<LogRecord>,
    next_seq: u64,
}

/// Cloneable handle: one clone is installed as a layer, another lives in
/// `AppState`.
#[derive(Clone)]
pub struct LogBuffer {
    inner: Arc<Mutex<Ring>>,
    capacity: usize,
}

impl LogBuffer {
    pub fn new(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        Self {
            inner: Arc::new(Mutex::new(Ring {
                records: VecDeque::with_capacity(capacity),
                next_seq: 0,
            })),
            capacity,
        }
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    fn push(&self, mut record: LogRecord) {
        // A poisoned lock is recovered rather than propagated: a panic in some
        // other thread must not take logging down with it.
        let mut ring = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        record.seq = ring.next_seq;
        ring.next_seq += 1;
        if ring.records.len() == self.capacity {
            ring.records.pop_front();
        }
        ring.records.push_back(record);
    }

    /// Newest first, optionally filtered to a minimum severity. `Level` orders
    /// `ERROR < WARN < INFO < DEBUG < TRACE`, so "at least this severe" is `<=`.
    pub fn snapshot(&self, min_level: Option<Level>, limit: usize) -> Vec<LogRecord> {
        let ring = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let mut out = Vec::with_capacity(limit.min(ring.records.len()));
        out.extend(
            ring.records
                .iter()
                .rev()
                .filter(|r| min_level.is_none_or(|k| r.level <= k))
                .take(limit)
                .cloned(),
        );
        out
    }
}

/// Splits an event into its rendered message and its structured fields.
#[derive(Default)]
struct Visitor {
    message: String,
    fields: String,
}

impl Visitor {
    fn add(&mut self, field: &Field, value: std::fmt::Arguments<'_>) {
        if field.name() == "message" {
            let _ = write!(self.message, "{value}");
        } else {
            if !self.fields.is_empty() {
                self.fields.push(' ');
            }
            let _ = write!(self.fields, "{}={value}", field.name());
        }
    }
}

impl Visit for Visitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.add(field, format_args!("{value:?}"));
    }

    /// Not via `record_debug`, which would quote the value.
    fn record_str(&mut self, field: &Field, value: &str) {
        self.add(field, format_args!("{value}"));
    }
}

impl<S: Subscriber> Layer<S> for LogBuffer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let mut visitor = Visitor::default();
        event.record(&mut visitor);
        let meta = event.metadata();
        self.push(LogRecord {
            seq: 0,
            ts: Utc::now(),
            level: *meta.level(),
            target: meta.target(),
            message: visitor.message,
            fields: visitor.fields,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing_subscriber::layer::SubscriberExt;

    fn record(level: Level, message: &str) -> LogRecord {
        LogRecord {
            seq: 0,
            ts: Utc::now(),
            level,
            target: "test",
            message: message.into(),
            fields: String::new(),
        }
    }

    /// The buffer is the memory ceiling. Without eviction a long-running
    /// gateway would grow one allocation per log line until it was killed.
    #[test]
    fn the_oldest_record_is_evicted_at_capacity() {
        let buf = LogBuffer::new(3);
        for i in 0..5 {
            buf.push(record(Level::INFO, &format!("line {i}")));
        }
        let recs = buf.snapshot(None, 10);
        assert_eq!(recs.len(), 3);
        assert_eq!(recs[0].message, "line 4", "newest first");
        assert_eq!(recs[2].message, "line 2", "lines 0 and 1 evicted");
        assert_eq!(recs[0].seq, 4, "seq survives eviction, so it is stable");
    }

    /// Guards the direction of `Level`'s ordering, which is the whole filter.
    #[test]
    fn a_level_filter_keeps_everything_at_least_that_severe() {
        let buf = LogBuffer::new(16);
        for level in [Level::ERROR, Level::WARN, Level::INFO, Level::DEBUG] {
            buf.push(record(level, level.as_str()));
        }
        let warn: Vec<_> = buf
            .snapshot(Some(Level::WARN), 10)
            .into_iter()
            .map(|r| r.level)
            .collect();
        assert_eq!(
            warn,
            [Level::WARN, Level::ERROR],
            "newest first, INFO and DEBUG cut"
        );
        assert_eq!(buf.snapshot(None, 10).len(), 4, "no filter keeps all");
    }

    /// The limit has to apply after the filter, or asking for the last 2 errors
    /// in a busy INFO stream returns nothing.
    #[test]
    fn the_limit_applies_after_the_filter() {
        let buf = LogBuffer::new(16);
        buf.push(record(Level::ERROR, "first"));
        for i in 0..10 {
            buf.push(record(Level::INFO, &format!("noise {i}")));
        }
        buf.push(record(Level::ERROR, "second"));
        let errors = buf.snapshot(Some(Level::ERROR), 2);
        assert_eq!(errors.len(), 2);
        assert_eq!(errors[0].message, "second");
        assert_eq!(errors[1].message, "first");
    }

    /// The layer has to pull the rendered message out of the event's fields;
    /// this is the only part that depends on how `tracing` records a macro.
    #[test]
    fn an_event_is_captured_with_its_rendered_message() {
        let buf = LogBuffer::new(8);
        let subscriber = tracing_subscriber::registry().with(buf.clone());
        tracing::subscriber::with_default(subscriber, || {
            tracing::warn!("queue at {}%", 91);
        });
        let recs = buf.snapshot(None, 10);
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].level, Level::WARN);
        assert_eq!(recs[0].message, "queue at 91%", "interpolated, unquoted");
    }

    /// `db.rs` puts the database name in a field and not in the message, so a
    /// visitor that kept only the message would render the migration warnings
    /// naming no database — the detail an operator is reading them for.
    #[test]
    fn structured_fields_are_kept_beside_the_message() {
        let buf = LogBuffer::new(8);
        let subscriber = tracing_subscriber::registry().with(buf.clone());
        tracing::subscriber::with_default(subscriber, || {
            tracing::warn!(
                database = "sungrow_ben_wired",
                elapsed_ms = 1_373_175,
                "schema migrated"
            );
        });
        let recs = buf.snapshot(None, 10);
        assert_eq!(recs[0].message, "schema migrated");
        assert_eq!(
            recs[0].fields, "database=sungrow_ben_wired elapsed_ms=1373175",
            "unquoted, and the message field is not repeated here"
        );
    }
}
