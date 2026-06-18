//! Резюмируемое состояние пайплайна в PostgreSQL (async, sqlx). Только статусы
//! стадий — сами данные живут в Sink (ФС/S3). Postgres выбран ради
//! распределённого сбора несколькими воркерами по одной очереди.

use detox_parser_core::error::{Error, Result};
use detox_parser_core::types::{Candidate, Platform, Seed, VideoId};
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS videos (
    platform      TEXT NOT NULL,
    id            TEXT NOT NULL,
    url           TEXT NOT NULL,
    discovered_at BIGINT NOT NULL,
    source        TEXT,
    query         TEXT,
    channel_id    TEXT,
    domain        TEXT,
    duration_s    DOUBLE PRECISION,
    status_meta   TEXT NOT NULL DEFAULT 'pending',
    status_media  TEXT NOT NULL DEFAULT 'pending',
    retries_meta  INT  NOT NULL DEFAULT 0,
    retries_media INT  NOT NULL DEFAULT 0,
    meta_path     TEXT,
    media_path    TEXT,
    last_error    TEXT,
    updated_at    BIGINT NOT NULL,
    PRIMARY KEY (platform, id)
);
CREATE TABLE IF NOT EXISTS seeds (
    run_id   TEXT NOT NULL,
    platform TEXT NOT NULL,
    kind     TEXT NOT NULL,
    value    TEXT NOT NULL,
    ts       BIGINT NOT NULL
);
CREATE TABLE IF NOT EXISTS expanded_channels (
    platform   TEXT NOT NULL,
    channel_id TEXT NOT NULL,
    hop        INT  NOT NULL,
    ts         BIGINT NOT NULL,
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

/// Строка очереди медиа: `(id, url, domain, duration_s)`.
pub type PendingMedia = (VideoId, String, Option<String>, Option<f64>);

pub struct Manifest {
    pool: PgPool,
}

fn map_err(e: sqlx::Error) -> Error {
    Error::Storage(e.to_string())
}

impl Manifest {
    /// Подключиться к Postgres и создать схему (идемпотентно).
    pub async fn connect(url: &str) -> Result<Self> {
        let pool = PgPoolOptions::new()
            .max_connections(8)
            .connect(url)
            .await
            .map_err(map_err)?;
        sqlx::raw_sql(SCHEMA).execute(&pool).await.map_err(map_err)?;
        Ok(Self { pool })
    }

    /// Зарегистрировать кандидата. Возвращает `true`, если он новый (дедуп).
    pub async fn upsert_candidate(&self, c: &Candidate) -> Result<bool> {
        let res = sqlx::query(
            "INSERT INTO videos (platform, id, url, discovered_at, source, query, updated_at)
             VALUES ($1, $2, $3, $4, $5, $6, $4)
             ON CONFLICT (platform, id) DO NOTHING",
        )
        .bind(c.video.platform.as_str())
        .bind(c.video.id.as_str())
        .bind(c.url.as_str())
        .bind(c.provenance.fetched_at)
        .bind(c.provenance.source.as_str())
        .bind(c.provenance.query.as_deref())
        .execute(&self.pool)
        .await
        .map_err(map_err)?;
        Ok(res.rows_affected() > 0)
    }

    pub async fn record_seed(&self, run_id: &str, seed: &Seed, ts: i64) -> Result<()> {
        let kind = format!("{:?}", seed.kind).to_lowercase();
        sqlx::query("INSERT INTO seeds (run_id, platform, kind, value, ts) VALUES ($1,$2,$3,$4,$5)")
            .bind(run_id)
            .bind(seed.platform.as_str())
            .bind(kind)
            .bind(seed.value.as_str())
            .bind(ts)
            .execute(&self.pool)
            .await
            .map_err(map_err)?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn mark_meta(
        &self,
        v: &VideoId,
        status: Status,
        meta_path: Option<&str>,
        channel_id: Option<&str>,
        domain: Option<&str>,
        duration_s: Option<f64>,
        err: Option<&str>,
        now: i64,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE videos SET status_meta=$3, meta_path=COALESCE($4, meta_path),
                channel_id=COALESCE($7, channel_id), domain=COALESCE($8, domain),
                duration_s=COALESCE($9, duration_s), last_error=$5, updated_at=$6,
                retries_meta = retries_meta + (CASE WHEN $3='failed' THEN 1 ELSE 0 END)
             WHERE platform=$1 AND id=$2",
        )
        .bind(v.platform.as_str())
        .bind(v.id.as_str())
        .bind(status.as_str())
        .bind(meta_path)
        .bind(err)
        .bind(now)
        .bind(channel_id)
        .bind(domain)
        .bind(duration_s)
        .execute(&self.pool)
        .await
        .map_err(map_err)?;
        Ok(())
    }

    /// Каналы собранных видео, которые ещё не расширяли (для многохопа).
    pub async fn unexpanded_channels(&self, platform: Platform, limit: usize) -> Result<Vec<String>> {
        let rows = sqlx::query(
            "SELECT DISTINCT channel_id FROM videos
             WHERE platform=$1 AND status_meta='ok' AND channel_id IS NOT NULL
               AND channel_id NOT IN
                   (SELECT channel_id FROM expanded_channels WHERE platform=$1)
             LIMIT $2",
        )
        .bind(platform.as_str())
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await
        .map_err(map_err)?;
        Ok(rows.iter().map(|r| r.get::<String, _>("channel_id")).collect())
    }

    pub async fn mark_channel_expanded(
        &self,
        platform: Platform,
        channel_id: &str,
        hop: u32,
        now: i64,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO expanded_channels (platform, channel_id, hop, ts)
             VALUES ($1,$2,$3,$4) ON CONFLICT (platform, channel_id) DO NOTHING",
        )
        .bind(platform.as_str())
        .bind(channel_id)
        .bind(hop as i32)
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(map_err)?;
        Ok(())
    }

    pub async fn mark_media(
        &self,
        v: &VideoId,
        status: Status,
        media_path: Option<&str>,
        err: Option<&str>,
        now: i64,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE videos SET status_media=$3, media_path=COALESCE($4, media_path),
                last_error=$5, updated_at=$6,
                retries_media = retries_media + (CASE WHEN $3='failed' THEN 1 ELSE 0 END)
             WHERE platform=$1 AND id=$2",
        )
        .bind(v.platform.as_str())
        .bind(v.id.as_str())
        .bind(status.as_str())
        .bind(media_path)
        .bind(err)
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(map_err)?;
        Ok(())
    }

    /// id, у которых метаданные ещё не собраны (pending или failed с запасом ретраев).
    pub async fn pending_meta(
        &self,
        platform: Platform,
        max_retries: u32,
        limit: usize,
    ) -> Result<Vec<(VideoId, String)>> {
        let rows = sqlx::query(
            "SELECT id, url FROM videos
             WHERE platform=$1 AND (status_meta='pending'
                   OR (status_meta='failed' AND retries_meta < $2))
             ORDER BY discovered_at LIMIT $3",
        )
        .bind(platform.as_str())
        .bind(max_retries as i32)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await
        .map_err(map_err)?;
        Ok(rows
            .iter()
            .map(|r| (VideoId::new(platform, r.get::<String, _>("id")), r.get::<String, _>("url")))
            .collect())
    }

    /// id, готовые к скачиванию медиа: метаданные ok, медиа ещё нет.
    /// Возвращает `(id, url, domain, duration_s)`.
    pub async fn pending_media(
        &self,
        platform: Platform,
        max_retries: u32,
        limit: usize,
    ) -> Result<Vec<PendingMedia>> {
        let rows = sqlx::query(
            "SELECT id, url, domain, duration_s FROM videos
             WHERE platform=$1 AND status_meta='ok'
               AND (status_media='pending'
                    OR (status_media='failed' AND retries_media < $2))
             ORDER BY discovered_at LIMIT $3",
        )
        .bind(platform.as_str())
        .bind(max_retries as i32)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await
        .map_err(map_err)?;
        Ok(rows
            .iter()
            .map(|r| {
                (
                    VideoId::new(platform, r.get::<String, _>("id")),
                    r.get::<String, _>("url"),
                    r.get::<Option<String>, _>("domain"),
                    r.get::<Option<f64>, _>("duration_s"),
                )
            })
            .collect())
    }

    /// Сколько медиа уже успешно скачано по каждому домену (для засева квот).
    pub async fn media_ok_by_domain(&self) -> Result<Vec<(String, i64)>> {
        let rows = sqlx::query(
            "SELECT COALESCE(domain, 'unknown') AS d, COUNT(*) AS n FROM videos
             WHERE status_media='ok' GROUP BY domain",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(map_err)?;
        Ok(rows
            .iter()
            .map(|r| (r.get::<String, _>("d"), r.get::<i64, _>("n")))
            .collect())
    }

    /// Сводка `(стадия, статус, count)` для команды status.
    pub async fn summary(&self) -> Result<Vec<(String, String, i64)>> {
        let mut out = vec![];
        for (stage, sql) in [
            ("meta", "SELECT status_meta AS s, COUNT(*) AS n FROM videos GROUP BY status_meta"),
            ("media", "SELECT status_media AS s, COUNT(*) AS n FROM videos GROUP BY status_media"),
        ] {
            let rows = sqlx::query(sql).fetch_all(&self.pool).await.map_err(map_err)?;
            for r in &rows {
                out.push((stage.to_string(), r.get::<String, _>("s"), r.get::<i64, _>("n")));
            }
        }
        Ok(out)
    }
}
