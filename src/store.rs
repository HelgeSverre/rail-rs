//! SQLite persistence. Yes, every RAIL instance has its own database. Redundancy.

use std::sync::{Mutex, MutexGuard, PoisonError};

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct Link {
    pub id: String,
    pub target: String,
    pub created_at: i64,
    pub mirrors: Vec<Mirror>,
}

/// One copy of a link, minted by one upstream.
#[derive(Debug, Clone, Serialize)]
pub struct Mirror {
    pub upstream: String,
    pub short_url: String,
    pub healthy: bool,
    pub latency_ms: Option<u64>,
    pub last_checked: Option<i64>,
    pub last_error: Option<String>,
}

impl Mirror {
    /// A freshly minted mirror is presumed healthy until the health checker says otherwise.
    pub fn new(upstream: impl Into<String>, short_url: impl Into<String>) -> Self {
        Self {
            upstream: upstream.into(),
            short_url: short_url.into(),
            healthy: true,
            latency_ms: None,
            last_checked: None,
            last_error: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct CheckOutcome {
    pub healthy: bool,
    pub latency_ms: Option<u64>,
    pub error: Option<String>,
    pub checked_at: i64,
}

pub struct Store {
    conn: Mutex<Connection>,
}

const SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS links (
        id         TEXT PRIMARY KEY,
        target     TEXT NOT NULL,
        created_at INTEGER NOT NULL
    );
    CREATE INDEX IF NOT EXISTS links_target ON links(target);
    CREATE TABLE IF NOT EXISTS mirrors (
        link_id      TEXT NOT NULL REFERENCES links(id),
        upstream     TEXT NOT NULL,
        short_url    TEXT NOT NULL,
        healthy      INTEGER NOT NULL DEFAULT 1,
        latency_ms   INTEGER,
        last_checked INTEGER,
        last_error   TEXT,
        PRIMARY KEY (link_id, upstream)
    );
";

impl Store {
    /// Open (or create) the database at `path`. `:memory:` works for tests.
    pub fn open(path: &str) -> Result<Self> {
        let conn = Connection::open(path).with_context(|| format!("opening database {path}"))?;
        conn.execute_batch(SCHEMA).context("creating schema")?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn conn(&self) -> MutexGuard<'_, Connection> {
        // A panic mid-query leaves nothing half-written that SQLite didn't already roll back.
        self.conn.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn insert(&self, link: &Link) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO links (id, target, created_at) VALUES (?1, ?2, ?3)",
            params![link.id, link.target, link.created_at],
        )?;
        for m in &link.mirrors {
            tx.execute(
                "INSERT INTO mirrors (link_id, upstream, short_url, healthy) VALUES (?1, ?2, ?3, ?4)",
                params![link.id, m.upstream, m.short_url, m.healthy],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn get(&self, id: &str) -> Result<Option<Link>> {
        let conn = self.conn();
        load(&conn, id)
    }

    pub fn find_by_target(&self, target: &str) -> Result<Option<Link>> {
        let conn = self.conn();
        let id: Option<String> = conn
            .query_row(
                "SELECT id FROM links WHERE target = ?1 ORDER BY created_at LIMIT 1",
                [target],
                |row| row.get(0),
            )
            .optional()?;
        id.map_or(Ok(None), |id| load(&conn, &id))
    }

    pub fn all(&self) -> Result<Vec<Link>> {
        let conn = self.conn();
        let ids = conn
            .prepare("SELECT id FROM links ORDER BY created_at DESC")?
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        ids.iter()
            .filter_map(|id| load(&conn, id).transpose())
            .collect()
    }

    pub fn record_check(
        &self,
        link_id: &str,
        upstream: &str,
        outcome: &CheckOutcome,
    ) -> Result<()> {
        self.conn().execute(
            "UPDATE mirrors SET healthy = ?3, latency_ms = ?4, last_checked = ?5, last_error = ?6
             WHERE link_id = ?1 AND upstream = ?2",
            params![
                link_id,
                upstream,
                outcome.healthy,
                outcome.latency_ms.and_then(|ms| i64::try_from(ms).ok()),
                outcome.checked_at,
                outcome.error,
            ],
        )?;
        Ok(())
    }
}

fn load(conn: &Connection, id: &str) -> Result<Option<Link>> {
    let Some((target, created_at)) = conn
        .query_row(
            "SELECT target, created_at FROM links WHERE id = ?1",
            [id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
        )
        .optional()?
    else {
        return Ok(None);
    };
    let mirrors = conn
        .prepare(
            "SELECT upstream, short_url, healthy, latency_ms, last_checked, last_error
             FROM mirrors WHERE link_id = ?1 ORDER BY upstream",
        )?
        .query_map([id], |row| {
            Ok(Mirror {
                upstream: row.get(0)?,
                short_url: row.get(1)?,
                healthy: row.get(2)?,
                latency_ms: row
                    .get::<_, Option<i64>>(3)?
                    .and_then(|ms| u64::try_from(ms).ok()),
                last_checked: row.get(4)?,
                last_error: row.get(5)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(Some(Link {
        id: id.to_owned(),
        target,
        created_at,
        mirrors,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stores_and_updates_mirrors() -> Result<()> {
        let store = Store::open(":memory:")?;
        let link = Link {
            id: "abc".into(),
            target: "https://example.com/".into(),
            created_at: 1,
            mirrors: vec![
                Mirror::new("is.gd", "https://is.gd/x"),
                Mirror::new("v.gd", "https://v.gd/y"),
            ],
        };
        store.insert(&link)?;
        store.record_check(
            "abc",
            "v.gd",
            &CheckOutcome {
                healthy: false,
                latency_ms: Some(42),
                error: Some("gone".into()),
                checked_at: 2,
            },
        )?;

        let loaded = store
            .find_by_target("https://example.com/")?
            .expect("link exists");
        assert_eq!(loaded.mirrors.len(), 2);
        let vgd = loaded
            .mirrors
            .iter()
            .find(|m| m.upstream == "v.gd")
            .expect("v.gd mirror");
        assert!(!vgd.healthy);
        assert_eq!(vgd.latency_ms, Some(42));
        assert!(store.get("nope")?.is_none());
        Ok(())
    }
}
