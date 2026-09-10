//! Capture-database schema bootstrap. The canonical schema lives in
//! schema/init_schema.sql and is embedded at compile time.
//! It lives in this crate because the gateway is the only program that applies
//! it: the capture server forwards over the ingest protocol rather than writing
//! to PostgreSQL itself.
//!
//! The file is split into individual statements (dollar-quote aware) and run one
//! at a time, for three reasons: an error names the statement that caused it,
//! psql meta-commands can be dropped on the way through, and the version row
//! stamped at the foot of the file only lands if everything before it did.
//!
//! Note it is *not* because a continuous aggregate needs its own transaction —
//! `WITH NO DATA`, which the schema uses, is fine inside one. What genuinely
//! cannot run in a transaction is `WITH DATA` and `refresh_continuous_aggregate`,
//! which is why the refresh is a separate step rather than part of a file.

use tokio_postgres::Client;

const INIT_SCHEMA: &str = include_str!("../schema/init_schema.sql");

/// The version `init_schema.sql` creates. [`MIGRATIONS`] takes an older database
/// up to it. Kept in step with the `schema_version` row that file inserts —
/// `the_schema_seeds_the_version_it_claims` holds the two together.
pub const SCHEMA_VERSION: i32 = 1;

pub struct Migration {
    pub version: i32,
    pub description: &'static str,
    sql: &'static str,
}

/// Each entry takes a database from `version - 1` to `version`. Embedded rather
/// than read from disk: the container has no schema directory, and an operator
/// running the same file by hand must be running the same bytes.
const MIGRATIONS: &[Migration] = &[Migration {
    version: 1,
    description: "capture_frame: one table per protocol, discriminated by protocol",
    sql: include_str!("../schema/migrations/0001_capture_frame.sql"),
}];

/// What version a database is at, or `None` when it holds no capture schema at
/// all and [`apply_capture_schema`] will create it at [`SCHEMA_VERSION`].
///
/// Three cases, because two of them predate the version table: a database with
/// `schema_version` answers for itself; one with `can_frame` as a *table* is
/// version 0; one with `capture_frame` was migrated before this scheme existed
/// and is version 1. That last case is why this fingerprints rather than
/// assuming an absent table means "old".
pub async fn detect_version(client: &Client) -> Result<Option<i32>, String> {
    let row = client
        .query_one(
            "SELECT to_regclass('public.schema_version') IS NOT NULL,
                    (SELECT relkind FROM pg_class
                      WHERE oid = to_regclass('public.can_frame')) = 'r'::\"char\",
                    to_regclass('public.capture_frame') IS NOT NULL",
            &[],
        )
        .await
        .map_err(|e| format!("schema version probe failed: {e}"))?;
    let (tracked, legacy, renamed): (bool, Option<bool>, bool) =
        (row.get(0), row.get(1), row.get(2));

    if tracked {
        let row = client
            .query_one(
                "SELECT coalesce(max(version), 0) FROM public.schema_version",
                &[],
            )
            .await
            .map_err(|e| format!("schema version read failed: {e}"))?;
        return Ok(Some(row.get::<_, i32>(0)));
    }
    Ok(match (legacy, renamed) {
        (Some(true), _) => Some(0),
        (_, true) => Some(SCHEMA_VERSION),
        _ => None,
    })
}

/// Bring a database up to [`SCHEMA_VERSION`], then apply the current schema.
/// `from` is what [`detect_version`] said; taking it as an argument keeps the
/// caller's probe from being repeated here.
///
/// Idempotent: a database already current runs no migration, and
/// `apply_capture_schema` is IF NOT EXISTS throughout.
pub async fn migrate(client: &Client, from: Option<i32>) -> Result<(), String> {
    let pending: Vec<&Migration> = match from {
        // Nothing here yet — init_schema creates it current.
        None => Vec::new(),
        Some(v) => MIGRATIONS.iter().filter(|m| m.version > v).collect(),
    };
    for m in &pending {
        tracing::warn!(
            version = m.version,
            "applying schema migration: {}",
            m.description
        );
        for stmt in split_statements(m.sql) {
            client
                .batch_execute(&stmt)
                .await
                .map_err(|e| format!("migration {} failed: {e}\nstatement: {stmt}", m.version))?;
        }
    }
    apply_capture_schema(client).await
}

/// Where a database's hourly rollup stands.
pub enum RollupState {
    /// No frames, so nothing to summarise and nothing a rebuild would achieve.
    Empty,
    /// The stored summaries do not reach back to the earliest frame. Rollup
    /// queries **omit** the uncovered span rather than recomputing it, so this is
    /// wrong answers, not slow ones.
    Incomplete,
    /// Covered from the first frame; `lag_secs` is the distance from the newest
    /// frame to the last stored bucket.
    Covered { lag_secs: i64 },
}

/// One query answering every question about the rollup.
///
/// Deliberately compares the *earliest* stored bucket with the earliest frame.
/// A count of materialised chunks cannot do this: the maintenance policy writes
/// a recent window and leaves a hole underneath, which shows up as one chunk and
/// a small lag — healthy on both of the obvious measures, and missing most of
/// the archive.
pub async fn rollup_status(client: &Client) -> Result<RollupState, String> {
    let row = client
        .query_one(
            "SELECT format('%I.%I', materialization_hypertable_schema, \
                                    materialization_hypertable_name) \
               FROM timescaledb_information.continuous_aggregates \
              WHERE view_schema = 'public' AND view_name = 'capture_frame_hourly'",
            &[],
        )
        .await
        .map_err(|e| format!("rollup lookup failed: {e}"))?;
    let mat: String = row.get(0);

    let row = client
        .query_one(
            &format!(
                "SELECT (SELECT min(ts) FROM public.capture_frame), \
                        (SELECT max(ts) FROM public.capture_frame), \
                        (SELECT min(bucket) FROM {mat}), \
                        (SELECT max(bucket) FROM {mat}), \
                        (SELECT min(ts) FROM public.capture_frame) \
                          < now() - INTERVAL '3 hours'"
            ),
            &[],
        )
        .await
        .map_err(|e| format!("rollup probe failed: {e}"))?;
    let first_ts: Option<std::time::SystemTime> = row.get(0);
    let last_ts: Option<std::time::SystemTime> = row.get(1);
    let first_bucket: Option<std::time::SystemTime> = row.get(2);
    let last_bucket: Option<std::time::SystemTime> = row.get(3);
    // Older than the policy's `start_offset`, so the policy can never reach back
    // that far by itself.
    let beyond_policy_reach: Option<bool> = row.get(4);

    let (Some(first_ts), Some(last_ts)) = (first_ts, last_ts) else {
        return Ok(RollupState::Empty);
    };

    match (first_bucket, last_bucket) {
        // Covered from the start: the only healthy shape.
        (Some(fb), Some(lb)) if fb <= first_ts => Ok(RollupState::Covered {
            lag_secs: last_ts
                .duration_since(lb)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0),
        }),
        // Stored summaries that start *after* the first frame: a hole beneath
        // them, and every query is already omitting it.
        (Some(_), Some(_)) => Ok(RollupState::Incomplete),
        // Nothing stored. Reads are correct for now — the watermark has not
        // moved, so everything comes from the raw table — but the first policy
        // run that has data to write opens the hole above. Only worth flagging
        // when the archive reaches further back than the policy can: a database
        // whose whole history is inside `start_offset` gets covered properly by
        // the next run.
        _ if beyond_policy_reach.unwrap_or(false) => Ok(RollupState::Incomplete),
        _ => Ok(RollupState::Covered { lag_secs: 0 }),
    }
}

/// Materialise the hourly rollup over its whole range.
///
/// Cannot run inside a transaction, which is why it is a statement of its own
/// rather than part of a migration file. Minutes on a very large archive —
/// measured at 16 s per 88 M compressed rows — and unavoidable after a
/// migration: see the note at the call site in `db.rs`.
pub async fn refresh_rollup(client: &Client) -> Result<(), String> {
    // Retried once, and only for a collision. Creating the aggregate also creates
    // its maintenance policy, and that job can be refreshing while this runs;
    // TimescaleDB refuses the second with 55P03 rather than blocking. Retrying
    // anything else would double the cost of a genuine failure — on a large
    // archive that is minutes of refresh run twice while the database is still
    // refusing traffic — and delay the error that says why.
    match refresh_rollup_once(client).await {
        Ok(()) => Ok(()),
        Err(e) if is_concurrent_refresh(&e) => {
            tracing::warn!("rollup refresh collided with the maintenance job, retrying: {e}");
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            refresh_rollup_once(client).await
        }
        Err(e) => Err(e),
    }
}

/// TimescaleDB's "could not refresh … due to a concurrent refresh" (55P03), and
/// the `tuple concurrently updated` the policy's job can raise against the same
/// catalogue rows. Matched on the message because the two differ by version.
fn is_concurrent_refresh(e: &str) -> bool {
    let e = e.to_ascii_lowercase();
    e.contains("concurrent refresh") || e.contains("concurrently updated")
}

/// The one definition of the refresh, shared with the psql post-step through
/// `the_migration_backfills_the_rollup`.
const REFRESH_SQL: &str =
    "CALL refresh_continuous_aggregate('public.capture_frame_hourly', NULL, NULL)";

async fn refresh_rollup_once(client: &Client) -> Result<(), String> {
    client.batch_execute(REFRESH_SQL).await.map_err(|e| {
        // tokio_postgres' Display is just "db error"; the server's message is
        // the only part worth reading.
        let detail = e
            .as_db_error()
            .map(|d| format!("{}: {}", d.severity(), d.message()))
            .unwrap_or_else(|| e.to_string());
        format!("rollup refresh failed: {detail}")
    })
}

/// Apply the full capture schema to an (empty or already-initialised)
/// database. Statements are idempotent (IF NOT EXISTS throughout).
pub async fn apply_capture_schema(client: &Client) -> Result<(), String> {
    for stmt in split_statements(INIT_SCHEMA) {
        client
            .batch_execute(&stmt)
            .await
            .map_err(|e| format!("schema statement failed: {e}\nstatement: {stmt}"))?;
    }
    Ok(())
}

/// Split an SQL script into statements on top-level semicolons, respecting
/// dollar-quoted bodies ($$ … $$ / $tag$ … $tag$), quoted strings and
/// line comments. Good enough for our own schema file; not a general parser.
///
/// psql meta-commands (`\set`, `\ir`, …) are dropped. The migrations are written
/// to be runnable by psql *and* by this, and the two need different things: psql
/// needs `ON_ERROR_STOP` and an `\ir` to pull in init_schema.sql, while this
/// stops on the first error by construction and applies that file itself.
pub fn split_statements(sql: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut chars = sql.char_indices().peekable();
    let bytes = sql.as_bytes();
    let mut dollar_tag: Option<String> = None;
    let mut in_single = false;
    let mut in_comment = false;
    // Only a backslash that opens a line is a meta-command; one inside a string
    // or a dollar-quoted body is data. Leading whitespace keeps the line "open".
    let mut at_line_start = true;

    while let Some((i, c)) = chars.next() {
        if in_comment {
            current.push(c);
            if c == '\n' {
                in_comment = false;
                at_line_start = true;
            }
            continue;
        }
        if let Some(tag) = &dollar_tag {
            current.push(c);
            if c == '$' && sql[i..].starts_with(tag.as_str()) {
                for _ in 0..tag.len() - 1 {
                    if let Some((_, c2)) = chars.next() {
                        current.push(c2);
                    }
                }
                dollar_tag = None;
            }
            continue;
        }
        if in_single {
            current.push(c);
            if c == '\'' {
                in_single = false;
            }
            continue;
        }
        if at_line_start && c == '\\' {
            for (_, c2) in chars.by_ref() {
                if c2 == '\n' {
                    break;
                }
            }
            continue;
        }
        if !c.is_whitespace() {
            at_line_start = false;
        } else if c == '\n' {
            at_line_start = true;
        }
        match c {
            '-' if bytes.get(i + 1) == Some(&b'-') => {
                in_comment = true;
                current.push(c);
            }
            '\'' => {
                in_single = true;
                current.push(c);
            }
            '$' => {
                // Possible dollar-quote opener: $tag$ where tag is [A-Za-z0-9_]*
                let rest = &sql[i + 1..];
                if let Some(end) = rest.find('$') {
                    let tag_body = &rest[..end];
                    if tag_body
                        .chars()
                        .all(|t| t.is_ascii_alphanumeric() || t == '_')
                    {
                        let tag = format!("${tag_body}$");
                        current.push(c);
                        for _ in 0..tag.len() - 1 {
                            if let Some((_, c2)) = chars.next() {
                                current.push(c2);
                            }
                        }
                        dollar_tag = Some(tag);
                        continue;
                    }
                }
                current.push(c);
            }
            ';' => {
                let stmt = current.trim();
                if !stmt.is_empty() && !is_only_comments(stmt) {
                    out.push(stmt.to_string());
                }
                current.clear();
            }
            _ => current.push(c),
        }
    }
    let stmt = current.trim();
    if !stmt.is_empty() && !is_only_comments(stmt) {
        out.push(stmt.to_string());
    }
    out
}

fn is_only_comments(stmt: &str) -> bool {
    stmt.lines().all(|l| {
        let l = l.trim();
        l.is_empty() || l.starts_with("--")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The ingest role the grants in init_schema.sql target. Lives here because
    /// it is the test's expectation of what PREAMBLE and those grants must
    /// agree on, not something the schema code itself reads.
    const INGEST_ROLE: &str = "wiretap";
    /// The protocol the compatibility views must filter to. A literal, like
    /// `INGEST_ROLE` above and for the same reason: derived from
    /// `sql::DEFAULT_PROTOCOL` it could not catch that constant drifting away
    /// from the `'can'` written into the schema text.
    const INGEST_PROTOCOL: &str = "can";

    /// psql needs `\set ON_ERROR_STOP` and `\ir`; this runner needs neither and
    /// cannot execute either. One file has to satisfy both, so the splitter drops
    /// them — and must drop *only* them.
    #[test]
    fn psql_meta_commands_are_dropped_but_sql_is_not() {
        let stmts = split_statements(
            "\\set ON_ERROR_STOP on\nSELECT 1;\n\\ir ../init_schema.sql\nSELECT 2;",
        );
        assert_eq!(stmts.len(), 2);
        assert!(stmts[0].contains("SELECT 1"));
        assert!(stmts[1].contains("SELECT 2"));
        assert!(!stmts
            .iter()
            .any(|s| s.contains("ON_ERROR_STOP") || s.contains("\\ir")));
    }

    /// A backslash is only a meta-command at the start of a line. Inside a string
    /// it is data — `decode('\\xCAFE', 'hex')` must survive intact.
    #[test]
    fn a_backslash_inside_a_string_is_left_alone() {
        let stmts = split_statements("INSERT INTO t VALUES ('a\\nb');\nSELECT 1;");
        assert_eq!(stmts.len(), 2);
        assert!(stmts[0].contains("'a\\nb'"));
    }

    /// The migration wraps its destructive half in a transaction and leaves the
    /// continuous aggregate outside one, because TimescaleDB refuses to create a
    /// CAGG inside a transaction. Dropping BEGIN/COMMIT would silently undo that.
    #[test]
    fn the_migration_keeps_its_transaction_boundaries() {
        // Comments preceding a statement are carried with it, so these end with
        // the keyword rather than being it.
        let stmts = split_statements(MIGRATIONS[0].sql);
        let ends = |s: &String, kw: &str| s.trim_end().ends_with(kw);
        assert!(stmts.iter().any(|s| ends(s, "BEGIN")), "BEGIN was dropped");
        assert!(
            stmts.iter().any(|s| ends(s, "COMMIT")),
            "COMMIT was dropped"
        );
        let commit = stmts.iter().position(|s| ends(s, "COMMIT")).unwrap();
        let cagg = stmts
            .iter()
            .position(|s| s.contains("DROP MATERIALIZED VIEW"))
            .expect("the rollup drop is in this migration");
        assert!(
            cagg > commit,
            "the aggregate work must sit outside the transaction"
        );
    }

    /// Two runners, one obligation: the gateway calls [`refresh_rollup`], the
    /// psql path reaches an `\ir`'d post-step. If either loses it, a migrated
    /// archive omits history rather than merely answering slowly.
    ///
    /// Asserts against `REFRESH_SQL` itself, not a restatement — an earlier
    /// version compared two copies of the text and stayed green when the
    /// gateway's call site was deleted.
    #[test]
    fn the_migration_backfills_the_rollup() {
        let post = include_str!("../schema/migrations/0001_capture_frame_post.sql");
        assert!(
            post.contains(REFRESH_SQL.trim_end_matches(';')),
            "the psql post-step no longer refreshes"
        );
        assert!(
            MIGRATIONS[0].sql.contains("0001_capture_frame_post.sql"),
            "the migration no longer includes its post step"
        );
        // Nothing above runs under Rust: the splitter drops `\ir`, and the
        // gateway refreshes at its own moment, after init_schema recreates the
        // aggregate.
        assert!(
            !split_statements(MIGRATIONS[0].sql)
                .iter()
                .any(|s| s.contains("refresh_continuous_aggregate")),
            "the gateway would run the refresh before the aggregate exists"
        );
    }

    /// The constant and the row `init_schema.sql` inserts have to agree, or a
    /// fresh database reports a version the code does not believe in.
    #[test]
    fn the_schema_seeds_the_version_it_claims() {
        assert!(
            INIT_SCHEMA.contains(&format!(
                "INSERT INTO public.schema_version (version, description)\n  VALUES ({SCHEMA_VERSION},"
            )),
            "init_schema.sql does not seed version {SCHEMA_VERSION}"
        );
    }

    /// Migrations must be contiguous from 1 and end at the version the schema
    /// creates, or `migrate` silently skips one.
    #[test]
    fn migrations_are_contiguous_up_to_the_current_version() {
        for (i, m) in MIGRATIONS.iter().enumerate() {
            assert_eq!(
                m.version,
                i as i32 + 1,
                "migration {} is out of order",
                m.version
            );
        }
        assert_eq!(
            MIGRATIONS.last().map(|m| m.version),
            Some(SCHEMA_VERSION),
            "the last migration must reach SCHEMA_VERSION"
        );
    }

    #[test]
    fn splits_simple_statements() {
        let stmts = split_statements("CREATE TABLE a (x int);\nCREATE TABLE b (y int);");
        assert_eq!(stmts.len(), 2);
        assert!(stmts[1].starts_with("CREATE TABLE b"));
    }

    #[test]
    fn keeps_dollar_quoted_bodies_intact() {
        let sql = "CREATE FUNCTION f() RETURNS void LANGUAGE plpgsql AS $$\n\
                   BEGIN RAISE NOTICE 'a;b'; END $$;\nSELECT 1;";
        let stmts = split_statements(sql);
        assert_eq!(stmts.len(), 2);
        assert!(stmts[0].contains("RAISE NOTICE 'a;b';"));
    }

    #[test]
    fn handles_tagged_dollar_quotes_and_comments() {
        let sql = "-- leading comment; with semicolon\n\
                   CREATE FUNCTION g() AS $fn$ SELECT 1; $fn$ LANGUAGE sql;\nSELECT 2;";
        let stmts = split_statements(sql);
        assert_eq!(stmts.len(), 2);
        assert!(stmts[0].contains("$fn$ SELECT 1; $fn$"));
    }

    #[test]
    fn embedded_schema_splits_and_contains_expected_objects() {
        let stmts = split_statements(INIT_SCHEMA);
        assert!(stmts
            .iter()
            .any(|s| s.contains("CREATE TABLE IF NOT EXISTS public.capture_frame")));
        assert!(stmts.iter().any(|s| s.contains("create_hypertable")));
        assert!(stmts.iter().any(|s| s.contains("capture_frame_hourly")));
        // The plpgsql function body must survive as a single statement
        let f = stmts
            .iter()
            .find(|s| s.contains("CREATE FUNCTION public.ingest_can_frame"))
            .expect("ingest function present");
        assert!(f.contains("INSERT INTO public.capture_frame"));
        assert!(f.contains("END $$"));
    }

    /// The CAN-only names this schema used before 2026-09-10 are kept as views,
    /// so a reader outside this repo — a psql session, a dashboard, a runbook —
    /// does not break on the rename. No Rust reads them, so nothing else in
    /// `cargo test` would notice them going missing; `parity_test.py` would, but
    /// it needs a live database and runs out of band.
    #[test]
    fn the_legacy_can_only_names_survive_as_filtered_views() {
        let stmts = split_statements(INIT_SCHEMA);
        for view in ["public.can_frame", "public.can_frame_hourly"] {
            assert!(
                stmts
                    .iter()
                    .any(|s| s.contains(&format!("CREATE OR REPLACE VIEW {view} AS"))),
                "{view} is no longer a compatibility view"
            );
        }
        // Universally quantified, like the grants check: any view reading the
        // shared table directly must filter, or it reports every protocol's
        // frames under a CAN-only name. A hand-listed pair cannot see a third
        // view losing its filter.
        for stmt in stmts.iter().filter(|s| {
            s.contains("CREATE OR REPLACE VIEW") && s.contains("FROM public.capture_frame")
        }) {
            assert!(
                stmt.contains(&format!("WHERE protocol = '{INGEST_PROTOCOL}'")),
                "a view reads capture_frame without filtering protocol: {stmt}"
            );
        }
    }

    /// The schema creates the role its own grants target. If the two drift, a
    /// pristine database gets a role nothing grants to and an ingest user with
    /// no privileges — the exact failure a rename can cause, and one that only
    /// shows up against a live cluster. The role block lives in the SQL rather
    /// than in this module because the migration's `\ir` runs that file with no
    /// Rust around it.
    #[test]
    fn the_schema_creates_the_role_its_grants_target() {
        // Statement-wise, not line-wise: one GRANT spans several lines.
        let stmts = split_statements(INIT_SCHEMA);
        assert!(
            stmts
                .iter()
                .any(|s| s.contains(&format!("CREATE ROLE {INGEST_ROLE} NOLOGIN"))),
            "schema does not create the {INGEST_ROLE} role"
        );
        let grants: Vec<&String> = stmts
            .iter()
            .filter(|s| s.trim_start().starts_with("GRANT "))
            .collect();
        assert!(!grants.is_empty(), "schema has no GRANT statements");
        for grant in grants {
            assert!(
                grant.contains(&format!("TO {}", INGEST_ROLE)),
                "grant does not target {}: {}",
                INGEST_ROLE,
                grant
            );
        }
    }
}
