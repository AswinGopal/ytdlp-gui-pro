use std::path::PathBuf;

use rusqlite::{Connection, OptionalExtension, params};

use crate::config;
use crate::types::JobSpec;

fn db_path() -> PathBuf {
    config::data_dir().join("history.db")
}

/// Opens the application database, creating or migrating the history table.
/// The download queue shares this file (see `queue`) so that finishing a job
/// can move it from the queue to the history in one transaction.
pub(crate) fn open() -> rusqlite::Result<Connection> {
    let conn = Connection::open(db_path())?;
    init(&conn)?;
    Ok(conn)
}

fn init(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS history (
            id        INTEGER PRIMARY KEY AUTOINCREMENT,
            title     TEXT NOT NULL,
            url       TEXT NOT NULL UNIQUE,
            timestamp INTEGER NOT NULL DEFAULT (strftime('%s', 'now')),
            spec      TEXT
        );",
    )?;

    // Databases created before the queue existed have no `spec` column.
    let has_spec: bool = conn.query_row(
        "SELECT COUNT(*) > 0 FROM pragma_table_info('history') WHERE name = 'spec'",
        [],
        |row| row.get(0),
    )?;
    if !has_spec {
        conn.execute_batch("ALTER TABLE history ADD COLUMN spec TEXT;")?;
    }
    Ok(())
}

/// A single history record returned to the frontend.
///
/// Typed fields prevent the fragile `"{title} - {url}"` string format that
/// `lastIndexOf(' - ')` in JS would mis-split whenever the title itself
/// contains ` - `.
///
/// `spec` is `None` for entries recorded before the queue existed; a
/// re-download of those uses the default quality.
#[derive(serde::Serialize)]
pub struct HistoryEntry {
    pub title: String,
    pub url: String,
    pub spec: Option<JobSpec>,
}

/// Upserts a completed download on `conn` (a `Transaction` derefs to one).
///
/// One entry exists per URL: re-downloading a known URL refreshes the
/// `title` (the video may have been renamed), the `spec` (it may have been
/// fetched at a different quality this time) and the `timestamp` (it sorts to
/// the top of the list as the most recent download).
pub(crate) fn log_entry_on(
    conn: &Connection,
    title: &str,
    url: &str,
    spec: &JobSpec,
) -> rusqlite::Result<()> {
    let spec_json = serde_json::to_string(spec)
        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
    conn.execute(
        "INSERT INTO history (title, url, spec) VALUES (?1, ?2, ?3)
         ON CONFLICT(url) DO UPDATE
           SET title     = excluded.title,
               spec      = excluded.spec,
               timestamp = strftime('%s', 'now')",
        params![title.trim(), url.trim(), spec_json],
    )?;
    Ok(())
}

fn entry_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<HistoryEntry> {
    let spec: Option<String> = row.get(2)?;
    Ok(HistoryEntry {
        title: row.get(0)?,
        url: row.get(1)?,
        // An unreadable spec degrades to "no spec" rather than hiding the entry.
        spec: spec.and_then(|s| serde_json::from_str(&s).ok()),
    })
}

pub fn read_history() -> Vec<HistoryEntry> {
    let conn = match open() {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let mut stmt = match conn.prepare(
        "SELECT title, url, spec FROM history ORDER BY timestamp DESC, id DESC",
    ) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    stmt.query_map([], entry_from_row)
        .map(|rows| rows.filter_map(|r| r.ok()).collect())
        .unwrap_or_default()
}

pub fn get_entry(url: &str) -> Option<HistoryEntry> {
    let conn = open().ok()?;
    conn.query_row(
        "SELECT title, url, spec FROM history WHERE url = ?1",
        params![url.trim()],
        entry_from_row,
    )
    .optional()
    .ok()
    .flatten()
}

pub fn delete_entry(url: &str) {
    if let Ok(conn) = open() {
        let _ = conn.execute("DELETE FROM history WHERE url = ?1", params![url.trim()]);
    }
}

pub fn clear_history() {
    if let Ok(conn) = open() {
        let _ = conn.execute("DELETE FROM history", []);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Preset, Quality, Scope};

    #[test]
    fn migrates_old_schema_and_upserts_one_entry_per_url() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE history (
                id INTEGER PRIMARY KEY AUTOINCREMENT, title TEXT NOT NULL,
                url TEXT NOT NULL UNIQUE,
                timestamp INTEGER NOT NULL DEFAULT (strftime('%s', 'now')));
             INSERT INTO history (title, url) VALUES ('old', 'u');",
        )
        .unwrap();
        init(&conn).unwrap();
        init(&conn).unwrap(); // idempotent

        let spec = JobSpec { quality: Quality::Preset(Preset::Mp3), scope: Scope::Full };
        log_entry_on(&conn, "new", "u", &spec).unwrap();

        let (count, title, stored): (i64, String, Option<String>) = conn
            .query_row("SELECT COUNT(*), title, spec FROM history", [], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .unwrap();
        assert_eq!((count, title.as_str()), (1, "new"));
        let back: JobSpec = serde_json::from_str(&stored.unwrap()).unwrap();
        assert_eq!(back, spec);
    }
}
