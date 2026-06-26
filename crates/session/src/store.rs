//! Персистентность идентичностей. Прогрев дорог — его нельзя терять между
//! запусками (а всплеск повторного прогрева сам выглядит бот-подобно).
//!
//! Store **локален воркеру** (ADR-0002): идентичность принадлежит тому процессу,
//! чей браузер её прогрел; шарить jar между воркерами/IP = гарантированный burn.
//! Поэтому здесь локальный SQLite, а не общий Postgres-манифест.
//!
//! Трейт отделяет логику пула от носителя — в тестах подменяется на in-memory.

use crate::identity::{CookieJar, Fingerprint, Identity, Lifecycle};
use detox_parser_core::error::{Error, Result};
use rusqlite::Connection;
use std::sync::Mutex;

/// Носитель идентичностей. Реализации: SQLite (боевая) и любой мок в тестах.
pub trait SessionStore: Send + Sync {
    fn load_all(&self) -> Result<Vec<Identity>>;
    fn upsert(&self, identity: &Identity) -> Result<()>;
    fn delete(&self, id: &str) -> Result<()>;
}

/// SQLite-хранилище. `fingerprint` и `jar` лежат как JSON-колонки — схема плоская
/// и человекочитаемая, миграции не нужны до несовместимых изменений.
pub struct SqliteSessionStore {
    conn: Mutex<Connection>,
}

impl SqliteSessionStore {
    /// Открыть/создать БД по пути и накатить схему.
    pub fn open(path: impl AsRef<std::path::Path>) -> Result<Self> {
        let conn = Connection::open(path).map_err(|e| Error::Storage(e.to_string()))?;
        Self::from_conn(conn)
    }

    /// In-memory БД (для тестов).
    pub fn in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory().map_err(|e| Error::Storage(e.to_string()))?;
        Self::from_conn(conn)
    }

    fn from_conn(conn: Connection) -> Result<Self> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS identities (
                id                TEXT PRIMARY KEY,
                proxy             TEXT NOT NULL,
                fingerprint       TEXT NOT NULL,
                jar               TEXT NOT NULL,
                lifecycle         TEXT NOT NULL,
                cooldown_until    INTEGER NOT NULL,
                req_count         INTEGER NOT NULL,
                consecutive_fails INTEGER NOT NULL,
                created_at        INTEGER NOT NULL,
                last_warmed_at    INTEGER NOT NULL
            );",
        )
        .map_err(|e| Error::Storage(e.to_string()))?;
        Ok(Self { conn: Mutex::new(conn) })
    }
}

impl SessionStore for SqliteSessionStore {
    fn load_all(&self) -> Result<Vec<Identity>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT id, proxy, fingerprint, jar, lifecycle, cooldown_until,
                        req_count, consecutive_fails, created_at, last_warmed_at
                 FROM identities",
            )
            .map_err(|e| Error::Storage(e.to_string()))?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?, // fingerprint json
                    r.get::<_, String>(3)?, // jar json
                    r.get::<_, String>(4)?, // lifecycle json-строка
                    r.get::<_, i64>(5)?,
                    r.get::<_, i64>(6)?,
                    r.get::<_, i64>(7)?,
                    r.get::<_, i64>(8)?,
                    r.get::<_, i64>(9)?,
                ))
            })
            .map_err(|e| Error::Storage(e.to_string()))?;

        let mut out = Vec::new();
        for row in rows {
            let (id, proxy, fp_s, jar_s, life_s, cd, req, fails, created, warmed) =
                row.map_err(|e| Error::Storage(e.to_string()))?;
            let fingerprint: Fingerprint = serde_json::from_str(&fp_s)?;
            let jar: CookieJar = serde_json::from_str(&jar_s)?;
            let lifecycle: Lifecycle = serde_json::from_str(&life_s)?;
            out.push(Identity {
                id,
                proxy,
                fingerprint,
                jar,
                lifecycle,
                cooldown_until: cd,
                req_count: req as u64,
                consecutive_fails: fails as u32,
                created_at: created,
                last_warmed_at: warmed,
            });
        }
        Ok(out)
    }

    fn upsert(&self, it: &Identity) -> Result<()> {
        let fp = serde_json::to_string(&it.fingerprint)?;
        let jar = serde_json::to_string(&it.jar)?;
        let life = serde_json::to_string(&it.lifecycle)?;
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO identities
                (id, proxy, fingerprint, jar, lifecycle, cooldown_until,
                 req_count, consecutive_fails, created_at, last_warmed_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT(id) DO UPDATE SET
                proxy=?2, fingerprint=?3, jar=?4, lifecycle=?5, cooldown_until=?6,
                req_count=?7, consecutive_fails=?8, created_at=?9, last_warmed_at=?10",
            rusqlite::params![
                it.id,
                it.proxy,
                fp,
                jar,
                life,
                it.cooldown_until,
                it.req_count as i64,
                it.consecutive_fails as i64,
                it.created_at,
                it.last_warmed_at,
            ],
        )
        .map_err(|e| Error::Storage(e.to_string()))?;
        Ok(())
    }

    fn delete(&self, id: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM identities WHERE id=?1", [id])
            .map_err(|e| Error::Storage(e.to_string()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::Fingerprint;

    fn ident(id: &str) -> Identity {
        let fp = Fingerprint {
            user_agent: "UA".into(),
            accept_language: "en".into(),
            timezone: "UTC".into(),
            locale: "en-US".into(),
            tls_profile: "chrome_131".into(),
        };
        let mut it = Identity::new(id, "http://sticky@h:1", fp, 100);
        it.jar.set("ttwid", "abc");
        it.mark_warmed(it.jar.clone(), 200);
        it
    }

    #[test]
    fn roundtrip_upsert_load_delete() {
        let store = SqliteSessionStore::in_memory().unwrap();
        let a = ident("a");
        store.upsert(&a).unwrap();

        let loaded = store.load_all().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0], a, "идентичность переживает roundtrip без потерь");

        // upsert тем же id обновляет, а не дублирует
        let mut a2 = a.clone();
        a2.proxy = "http://sticky2@h:2".into();
        store.upsert(&a2).unwrap();
        let loaded = store.load_all().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].proxy, "http://sticky2@h:2");

        store.delete("a").unwrap();
        assert!(store.load_all().unwrap().is_empty());
    }
}
