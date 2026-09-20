//! A user's annotations on one capture database: a moment or a span, with a
//! note. Protocol-agnostic — the point of one is to line it up against
//! whatever the archive holds at that time, whichever wire it came off.
//! Written through the HTTP API only; ingest never touches the table.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio_postgres::Client;

use crate::schema::db_error_detail;

/// Microseconds since the epoch throughout, as `time-bounds` and `frames`
/// already speak to the desktop.
#[derive(Debug, Serialize)]
pub struct Event {
    pub id: i64,
    pub ts_us: i64,
    pub duration_us: i64,
    pub note: String,
    pub created_at_us: i64,
    pub updated_at_us: i64,
}

const COLUMNS: &str = "id, ts, duration_us, note, created_at, updated_at";

impl Event {
    fn from_row(row: &tokio_postgres::Row) -> Self {
        let us = |col: &str| row.get::<_, DateTime<Utc>>(col).timestamp_micros();
        Self {
            id: row.get("id"),
            ts_us: us("ts"),
            duration_us: row.get("duration_us"),
            note: row.get("note"),
            created_at_us: us("created_at"),
            updated_at_us: us("updated_at"),
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct NewEvent {
    pub ts_us: i64,
    #[serde(default)]
    pub duration_us: i64,
    #[serde(default)]
    pub note: String,
}

/// Every field optional; an absent one is left as it was.
#[derive(Debug, Deserialize)]
pub struct EventPatch {
    pub ts_us: Option<i64>,
    pub duration_us: Option<i64>,
    pub note: Option<String>,
}

fn timestamp(ts_us: i64) -> Result<DateTime<Utc>, String> {
    DateTime::from_timestamp_micros(ts_us).ok_or_else(|| format!("ts_us {ts_us} is out of range"))
}

/// Checked here rather than left to the table's CHECK, so the client reads a
/// field name rather than a constraint name.
fn duration(duration_us: i64) -> Result<i64, String> {
    if duration_us < 0 {
        return Err("duration_us must not be negative".into());
    }
    Ok(duration_us)
}

/// Events in `[start, end]` on `ts`, oldest first. Both bounds optional, ISO
/// strings as the frame endpoints take — but inclusive at both ends where the
/// frame endpoints are half-open: a moment annotated at the very edge of a
/// window belongs to it, and events do not page.
pub async fn list(
    client: &Client,
    start: Option<String>,
    end: Option<String>,
    limit: u32,
) -> Result<Vec<Event>, String> {
    let rows = client
        .query(
            &format!(
                "SELECT {COLUMNS} FROM public.events \
                 WHERE ($1::text IS NULL OR ts >= ($1::text)::timestamptz) \
                   AND ($2::text IS NULL OR ts <= ($2::text)::timestamptz) \
                 ORDER BY ts, id LIMIT $3::int8"
            ),
            &[&start, &end, &i64::from(limit)],
        )
        .await
        .map_err(|e| format!("event list failed: {}", db_error_detail(&e)))?;
    Ok(rows.iter().map(Event::from_row).collect())
}

pub async fn create(client: &Client, new: &NewEvent) -> Result<Event, String> {
    let row = client
        .query_one(
            &format!(
                "INSERT INTO public.events (ts, duration_us, note) VALUES ($1, $2, $3) \
                 RETURNING {COLUMNS}"
            ),
            &[
                &timestamp(new.ts_us)?,
                &duration(new.duration_us)?,
                &new.note,
            ],
        )
        .await
        .map_err(|e| format!("event insert failed: {}", db_error_detail(&e)))?;
    Ok(Event::from_row(&row))
}

/// `None` when no event has that id.
pub async fn update(client: &Client, id: i64, patch: &EventPatch) -> Result<Option<Event>, String> {
    if patch.ts_us.is_none() && patch.duration_us.is_none() && patch.note.is_none() {
        return Err("nothing to change: give ts_us, duration_us or note".into());
    }
    let ts = patch.ts_us.map(timestamp).transpose()?;
    let duration_us = patch.duration_us.map(duration).transpose()?;
    let row = client
        .query_opt(
            &format!(
                "UPDATE public.events \
                 SET ts = coalesce($2::timestamptz, ts), \
                     duration_us = coalesce($3::int8, duration_us), \
                     note = coalesce($4::text, note), \
                     updated_at = now() \
                 WHERE id = $1 RETURNING {COLUMNS}"
            ),
            &[&id, &ts, &duration_us, &patch.note],
        )
        .await
        .map_err(|e| format!("event update failed: {}", db_error_detail(&e)))?;
    Ok(row.as_ref().map(Event::from_row))
}

/// Whether an event with that id existed to delete.
pub async fn delete(client: &Client, id: i64) -> Result<bool, String> {
    let n = client
        .execute("DELETE FROM public.events WHERE id = $1", &[&id])
        .await
        .map_err(|e| format!("event delete failed: {}", db_error_detail(&e)))?;
    Ok(n == 1)
}
