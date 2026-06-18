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
    channel_id    TEXT,                             -- заполняется после harvest (для expand)
    domain        TEXT,                             -- short|long, из duration (для квот)
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
-- какие каналы уже расширены (дедуп многохопа против survivorship bias)
CREATE TABLE IF NOT EXISTS expanded_channels (
    platform   TEXT NOT NULL,
    channel_id TEXT NOT NULL,
    hop        INTEGER NOT NULL,
    ts         INTEGER NOT NULL,
    PRIMARY KEY (platform, channel_id)
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
        // миграция старых БД: добавить колонки, если их нет (ошибку дубля глотаем)
        let _ = conn.execute("ALTER TABLE videos ADD COLUMN channel_id TEXT", []);
        let _ = conn.execute("ALTER TABLE videos ADD COLUMN domain TEXT", []);
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

    #[allow(clippy::too_many_arguments)]
    pub fn mark_meta(
        &self,
        v: &VideoId,
        status: Status,
        meta_path: Option<&str>,
        channel_id: Option<&str>,
        domain: Option<&str>,
        err: Option<&str>,
        now: i64,
    ) -> Result<()> {
        self.lock()
            .execute(
                "UPDATE videos SET status_meta=?3, meta_path=COALESCE(?4, meta_path),
                    channel_id=COALESCE(?7, channel_id), domain=COALESCE(?8, domain),
                    last_error=?5, updated_at=?6,
                    retries_meta = retries_meta + (CASE WHEN ?3='failed' THEN 1 ELSE 0 END)
                 WHERE platform=?1 AND id=?2",
                params![
                    v.platform.as_str(), v.id, status.as_str(),
                    meta_path, err, now, channel_id, domain
                ],
            )
            .map_err(map_err)?;
        Ok(())
    }

    /// Каналы собранных видео, которые ещё не расширяли (для многохопа).
    pub fn unexpanded_channels(&self, platform: Platform, limit: usize) -> Result<Vec<String>> {
        let conn = self.lock();
        let mut stmt = conn
            .prepare(
                "SELECT DISTINCT channel_id FROM videos
                 WHERE platform=?1 AND status_meta='ok' AND channel_id IS NOT NULL
                   AND channel_id NOT IN
                       (SELECT channel_id FROM expanded_channels WHERE platform=?1)
                 LIMIT ?2",
            )
            .map_err(map_err)?;
        let rows = stmt
            .query_map(params![platform.as_str(), limit as i64], |r| r.get::<_, String>(0))
            .map_err(map_err)?;
        rows.collect::<std::result::Result<_, _>>().map_err(map_err)
    }

    pub fn mark_channel_expanded(
        &self,
        platform: Platform,
        channel_id: &str,
        hop: u32,
        now: i64,
    ) -> Result<()> {
        self.lock()
            .execute(
                "INSERT OR IGNORE INTO expanded_channels (platform, channel_id, hop, ts)
                 VALUES (?1, ?2, ?3, ?4)",
                params![platform.as_str(), channel_id, hop, now],
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
    /// Возвращает `(id, url, domain)` — домен нужен для квот.
    pub fn pending_media(
        &self,
        platform: Platform,
        max_retries: u32,
        limit: usize,
    ) -> Result<Vec<(VideoId, String, Option<String>)>> {
        let conn = self.lock();
        let mut stmt = conn
            .prepare(
                "SELECT id, url, domain FROM videos
                 WHERE platform=?1 AND status_meta='ok'
                   AND (status_media='pending'
                        OR (status_media='failed' AND retries_media < ?2))
                 ORDER BY discovered_at LIMIT ?3",
            )
            .map_err(map_err)?;
        let rows = stmt
            .query_map(params![platform.as_str(), max_retries, limit as i64], |r| {
                Ok((
                    VideoId::new(platform, r.get::<_, String>(0)?),
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<String>>(2)?,
                ))
            })
            .map_err(map_err)?;
        rows.collect::<std::result::Result<_, _>>().map_err(map_err)
    }

    /// Сколько медиа уже успешно скачано по каждому домену (для засева квот).
    pub fn media_ok_by_domain(&self) -> Result<Vec<(String, i64)>> {
        let conn = self.lock();
        let mut stmt = conn
            .prepare(
                "SELECT COALESCE(domain, 'unknown'), COUNT(*) FROM videos
                 WHERE status_media='ok' GROUP BY domain",
            )
            .map_err(map_err)?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
            .map_err(map_err)?;
        rows.collect::<std::result::Result<_, _>>().map_err(map_err)
    }

    #[cfg(test)]
    fn count_source(&self, like: &str) -> i64 {
        self.lock()
            .query_row("SELECT COUNT(*) FROM videos WHERE source LIKE ?1", [like], |r| {
                r.get(0)
            })
            .unwrap()
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

#[cfg(test)]
mod tests {
    use super::*;
    use detox_parser_core::types::{Candidate, Provenance, VideoId};

    fn candidate(id: &str, source: &str) -> Candidate {
        Candidate {
            video: VideoId::new(Platform::Youtube, id),
            url: format!("https://yt/{id}"),
            provenance: Provenance {
                source: source.into(),
                query: None,
                rank: None,
                fetched_at: 0,
                extractor: "test".into(),
                extractor_version: None,
            },
        }
    }

    #[test]
    fn dedup_on_upsert() {
        let m = Manifest::open(":memory:").unwrap();
        let c = candidate("v1", "query:x");
        assert!(m.upsert_candidate(&c).unwrap(), "first insert is new");
        assert!(!m.upsert_candidate(&c).unwrap(), "second insert is dedup'd");
    }

    #[test]
    fn multihop_channel_expansion_tracking() {
        let m = Manifest::open(":memory:").unwrap();

        // hop 0: два видео из поиска, оба окажутся одного канала
        for id in ["v1", "v2"] {
            assert!(m.upsert_candidate(&candidate(id, "query:x")).unwrap());
        }
        let v1 = VideoId::new(Platform::Youtube, "v1");
        let v2 = VideoId::new(Platform::Youtube, "v2");
        m.mark_meta(&v1, Status::Ok, Some("p1"), Some("UC_channelA"), Some("long"), None, 1).unwrap();
        m.mark_meta(&v2, Status::Ok, Some("p2"), Some("UC_channelA"), Some("short"), None, 1).unwrap();

        // ровно один уникальный нерасширённый канал
        assert_eq!(
            m.unexpanded_channels(Platform::Youtube, 10).unwrap(),
            vec!["UC_channelA".to_string()]
        );

        // расширяем канал → добавляем его аплоад из «хвоста» (low-engagement)
        assert!(m.upsert_candidate(&candidate("v3", "channel:UC_channelA")).unwrap());
        m.mark_channel_expanded(Platform::Youtube, "UC_channelA", 1, 2).unwrap();

        // канал больше не возвращается (дедуп расширения — цикл сходится)
        assert!(m.unexpanded_channels(Platform::Youtube, 10).unwrap().is_empty());

        // в выборке появились видео и из поиска, и из расширения канала
        assert_eq!(m.count_source("query:%"), 2);
        assert_eq!(m.count_source("channel:%"), 1);
    }

    #[test]
    fn unharvested_channels_are_not_expanded() {
        let m = Manifest::open(":memory:").unwrap();
        assert!(m.upsert_candidate(&candidate("v1", "query:x")).unwrap());
        // метаданных ещё нет → channel_id неизвестен → расширять нечего
        assert!(m.unexpanded_channels(Platform::Youtube, 10).unwrap().is_empty());
    }

    #[test]
    fn media_quota_counts_and_domain_passthrough() {
        let m = Manifest::open(":memory:").unwrap();
        for id in ["s1", "l1", "l2"] {
            assert!(m.upsert_candidate(&candidate(id, "query:x")).unwrap());
        }
        let s1 = VideoId::new(Platform::Youtube, "s1");
        let l1 = VideoId::new(Platform::Youtube, "l1");
        let l2 = VideoId::new(Platform::Youtube, "l2");
        m.mark_meta(&s1, Status::Ok, Some("p"), None, Some("short"), None, 1).unwrap();
        m.mark_meta(&l1, Status::Ok, Some("p"), None, Some("long"), None, 1).unwrap();
        m.mark_meta(&l2, Status::Ok, Some("p"), None, Some("long"), None, 1).unwrap();

        // pending_media отдаёт домен — по нему квота решает скачивать или нет
        let pending = m.pending_media(Platform::Youtube, 2, 10).unwrap();
        assert_eq!(pending.len(), 3);
        assert!(pending.iter().all(|(_, _, d)| d.is_some()));

        // засев квоты считает уже скачанное по доменам
        m.mark_media(&s1, Status::Ok, Some("s1.mp4"), None, 2).unwrap();
        m.mark_media(&l1, Status::Ok, Some("l1.mp4"), None, 2).unwrap();
        let counts: std::collections::HashMap<_, _> =
            m.media_ok_by_domain().unwrap().into_iter().collect();
        assert_eq!(counts.get("short"), Some(&1));
        assert_eq!(counts.get("long"), Some(&1));
    }
}
