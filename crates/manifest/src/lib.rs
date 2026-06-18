//! Резюмируемое состояние пайплайна в SQLite. Только статусы стадий —
//! сами данные живут на ФС (принцип ml/etl.md). API синхронный: вызовы дёшевы
//! и делаются между await в pipeline.

use detox_parser_core::error::{Error, Result};
use detox_parser_core::types::{Candidate, Platform, Seed, VideoId};
use rusqlite::{params, Connection};
use std::path::Path;
use std::sync::Mutex;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS videos (
    platform      TEXT NOT NULL,
    id            TEXT NOT NULL,
    url           TEXT NOT NULL,
    discovered_at INTEGER NOT NULL,
    source        TEXT,
    query         TEXT,
    status_meta   TEXT NOT NULL DEFAULT 'pending',  -- pending|ok|failed|filtered
    status_media  TEXT NOT NULL DEFAULT 'pending',  -- pending|ok|failed|skipped
    retries_meta  INTEGER NOT NULL DEFAULT 0,
    retries_media INTEGER NOT NULL DEFAULT 0,
    meta_path     TEXT,
    media_path    TEXT,
    last_error    TEXT,
    updated_at    INTEGER NOT NULL,
    PRIMARY KEY (platform, id)
);
CREATE TABLE IF NOT EXISTS seeds (
    run_id   TEXT NOT NULL,
    platform TEXT NOT NULL,
    kind     TEXT NOT NULL,
    value    TEXT NOT NULL,
    ts       INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_videos_meta  ON videos(platform, status_meta);
CREATE INDEX IF NOT EXISTS idx_videos_media ON videos(platform, status_media);
"#;

/// Статус стадии.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Pending,
    Ok,
    Failed,
    Filtered,
    Skipped,
}

impl Status {
    fn as_str(&self) -> &'static str {
        match self {
            Status::Pending => "pending",
            Status::Ok => "ok",
            Status::Failed => "failed",
            Status::Filtered => "filtered",
            Status::Skipped => "skipped",
        }
    }
}

pub struct Manifest {
    conn: Mutex<Connection>,
}

fn map_err(e: rusqlite::Error) -> Error {
    Error::Storage(e.to_string())
}

impl Manifest {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let conn = Connection::open(path).map_err(map_err)?;
        conn.execute_batch(SCHEMA).map_err(map_err)?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().expect("manifest mutex poisoned")
    }

    /// Зарегистрировать кандидата. Возвращает `true`, если он новый (дедуп).
    pub fn upsert_candidate(&self, c: &Candidate) -> Result<bool> {
        let conn = self.lock();
        let changed = conn
            .execute(
                "INSERT OR IGNORE INTO videos
                   (platform, id, url, discovered_at, source, query, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?4)",
                params![
                    c.video.platform.as_str(),
                    c.video.id,
                    c.url,
                    c.provenance.fetched_at,
                    c.provenance.source,
                    c.provenance.query,
                ],
            )
            .map_err(map_err)?;
        Ok(changed > 0)
    }

    pub fn record_seed(&self, run_id: &str, seed: &Seed, ts: i64) -> Result<()> {
        let kind = format!("{:?}", seed.kind).to_lowercase();
        self.lock()
            .execute(
                "INSERT INTO seeds (run_id, platform, kind, value, ts) VALUES (?1,?2,?3,?4,?5)",
                params![run_id, seed.platform.as_str(), kind, seed.value, ts],
            )
            .map_err(map_err)?;
        Ok(())
    }

    pub fn mark_meta(
        &self,
        v: &VideoId,
        status: Status,
        meta_path: Option<&str>,
        err: Option<&str>,
        now: i64,
    ) -> Result<()> {
        self.lock()
            .execute(
                "UPDATE videos SET status_meta=?3, meta_path=COALESCE(?4, meta_path),
                    last_error=?5, updated_at=?6,
                    retries_meta = retries_meta + (CASE WHEN ?3='failed' THEN 1 ELSE 0 END)
                 WHERE platform=?1 AND id=?2",
                params![v.platform.as_str(), v.id, status.as_str(), meta_path, err, now],
            )
            .map_err(map_err)?;
        Ok(())
    }

    pub fn mark_media(
        &self,
        v: &VideoId,
        status: Status,
        media_path: Option<&str>,
        err: Option<&str>,
        now: i64,
    ) -> Result<()> {
        self.lock()
            .execute(
                "UPDATE videos SET status_media=?3, media_path=COALESCE(?4, media_path),
                    last_error=?5, updated_at=?6,
                    retries_media = retries_media + (CASE WHEN ?3='failed' THEN 1 ELSE 0 END)
                 WHERE platform=?1 AND id=?2",
                params![v.platform.as_str(), v.id, status.as_str(), media_path, err, now],
            )
            .map_err(map_err)?;
        Ok(())
    }

    /// id, у которых метаданные ещё не собраны (pending или failed с запасом ретраев).
    pub fn pending_meta(
        &self,
        platform: Platform,
        max_retries: u32,
        limit: usize,
    ) -> Result<Vec<(VideoId, String)>> {
        let conn = self.lock();
        let mut stmt = conn
            .prepare(
                "SELECT id, url FROM videos
                 WHERE platform=?1 AND (status_meta='pending'
                       OR (status_meta='failed' AND retries_meta < ?2))
                 ORDER BY discovered_at LIMIT ?3",
            )
            .map_err(map_err)?;
        let rows = stmt
            .query_map(params![platform.as_str(), max_retries, limit as i64], |r| {
                Ok((VideoId::new(platform, r.get::<_, String>(0)?), r.get::<_, String>(1)?))
            })
            .map_err(map_err)?;
        rows.collect::<std::result::Result<_, _>>().map_err(map_err)
    }

    /// id, готовые к скачиванию медиа: метаданные ok, медиа ещё нет.
    pub fn pending_media(
        &self,
        platform: Platform,
        max_retries: u32,
        limit: usize,
    ) -> Result<Vec<(VideoId, String)>> {
        let conn = self.lock();
        let mut stmt = conn
            .prepare(
                "SELECT id, url FROM videos
                 WHERE platform=?1 AND status_meta='ok'
                   AND (status_media='pending'
                        OR (status_media='failed' AND retries_media < ?2))
                 ORDER BY discovered_at LIMIT ?3",
            )
            .map_err(map_err)?;
        let rows = stmt
            .query_map(params![platform.as_str(), max_retries, limit as i64], |r| {
                Ok((VideoId::new(platform, r.get::<_, String>(0)?), r.get::<_, String>(1)?))
            })
            .map_err(map_err)?;
        rows.collect::<std::result::Result<_, _>>().map_err(map_err)
    }

    /// Сводка `(статус_стадии, стадия, count)` для команды status.
    pub fn summary(&self) -> Result<Vec<(String, String, i64)>> {
        let conn = self.lock();
        let mut out = vec![];
        for (stage, col) in [("meta", "status_meta"), ("media", "status_media")] {
            let sql = format!("SELECT {col}, COUNT(*) FROM videos GROUP BY {col}");
            let mut stmt = conn.prepare(&sql).map_err(map_err)?;
            let rows = stmt
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
                .map_err(map_err)?;
            for row in rows {
                let (status, n) = row.map_err(map_err)?;
                out.push((stage.to_string(), status, n));
            }
        }
        Ok(out)
    }
}
