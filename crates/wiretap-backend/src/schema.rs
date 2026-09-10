//! Capture-database schema bootstrap. The canonical schema lives in
//! schema/init_schema.sql and is embedded at compile time.
//! It lives in this crate because the gateway is the only program that applies
//! it: the capture server forwards over the ingest protocol rather than writing
//! to PostgreSQL itself.
//!
//! TimescaleDB continuous aggregates cannot be created inside a transaction
//! block, and the simple-protocol batch executor runs a multi-statement
//! string as one implicit transaction — so the file is split into individual
//! statements (dollar-quote aware) and executed one at a time.

use tokio_postgres::Client;

const INIT_SCHEMA: &str = include_str!("../schema/init_schema.sql");

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
pub fn split_statements(sql: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut chars = sql.char_indices().peekable();
    let bytes = sql.as_bytes();
    let mut dollar_tag: Option<String> = None;
    let mut in_single = false;
    let mut in_comment = false;

    while let Some((i, c)) = chars.next() {
        if in_comment {
            current.push(c);
            if c == '\n' {
                in_comment = false;
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
