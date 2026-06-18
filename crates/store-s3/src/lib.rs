//! S3-совместимый Bronze-sink (S3 / MinIO / Cloudflare R2).
//!
//! Раскладка ключей зеркалит ФС (`raw/meta/<id>.json`, `normalized/...`,
//! `raw/videos/...`), так что DuckDB/Polars читают бакет так же, как локальную
//! папку. Медиа yt-dlp пишет в локальный staging, затем оно загружается и
//! локальная копия удаляется. Креды — из env (AWS_ACCESS_KEY_ID/SECRET).

use aws_sdk_s3::config::{BehaviorVersion, Credentials, Region};
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::Client;
use detox_parser_core::error::{Error, Result};
use detox_parser_core::traits::Sink;
use detox_parser_core::types::*;
use std::path::PathBuf;
use tracing::debug;

/// Настройки подключения к объектному хранилищу.
#[derive(Debug, Clone)]
pub struct S3Settings {
    pub bucket: String,
    pub region: String,
    /// Кастомный endpoint для MinIO/R2 (None = AWS S3).
    pub endpoint: Option<String>,
    /// Префикс ключей внутри бакета.
    pub prefix: String,
    /// Path-style адресация (нужно для MinIO).
    pub path_style: bool,
    /// Локальный staging для медиа и discovery-логов.
    pub staging_dir: String,
}

pub struct S3Store {
    client: Client,
    bucket: String,
    prefix: String,
    staging: PathBuf,
}

fn s3_err<E: std::fmt::Display>(ctx: &str, e: E) -> Error {
    Error::Storage(format!("s3 {ctx}: {e}"))
}

impl S3Store {
    pub async fn new(cfg: S3Settings) -> Result<Self> {
        let access = std::env::var("AWS_ACCESS_KEY_ID")
            .map_err(|_| Error::Config("AWS_ACCESS_KEY_ID не задан".into()))?;
        let secret = std::env::var("AWS_SECRET_ACCESS_KEY")
            .map_err(|_| Error::Config("AWS_SECRET_ACCESS_KEY не задан".into()))?;
        let token = std::env::var("AWS_SESSION_TOKEN").ok();
        let creds = Credentials::new(access, secret, token, None, "env");

        let mut builder = aws_sdk_s3::config::Builder::new()
            .behavior_version(BehaviorVersion::latest())
            .region(Region::new(cfg.region))
            .credentials_provider(creds)
            .force_path_style(cfg.path_style);
        if let Some(ep) = cfg.endpoint {
            builder = builder.endpoint_url(ep);
        }
        let client = Client::from_conf(builder.build());

        let staging = PathBuf::from(&cfg.staging_dir);
        tokio::fs::create_dir_all(staging.join("discovery")).await?;

        Ok(Self { client, bucket: cfg.bucket, prefix: cfg.prefix, staging })
    }

    fn key(&self, rel: &str) -> String {
        if self.prefix.is_empty() {
            rel.to_string()
        } else {
            format!("{}/{}", self.prefix.trim_end_matches('/'), rel)
        }
    }

    async fn put_bytes(&self, key: &str, bytes: Vec<u8>) -> Result<()> {
        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .body(ByteStream::from(bytes))
            .send()
            .await
            .map_err(|e| s3_err("put_object", e))?;
        debug!(bucket = %self.bucket, key, "s3 put");
        Ok(())
    }

    async fn put_file(&self, key: &str, path: &std::path::Path) -> Result<()> {
        let body = ByteStream::from_path(path).await.map_err(|e| s3_err("read file", e))?;
        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .body(body)
            .send()
            .await
            .map_err(|e| s3_err("put_object file", e))?;
        Ok(())
    }
}

#[async_trait::async_trait]
impl Sink for S3Store {
    async fn put_raw(&self, extract: &RawExtract) -> Result<RawRef> {
        let key = self.key(&format!("raw/meta/{}.json", extract.video.id));
        self.put_bytes(&key, serde_json::to_vec(&extract.raw)?).await?;
        Ok(RawRef { path: format!("s3://{}/{}", self.bucket, key) })
    }

    async fn put_normalized(&self, video: &NormalizedVideo) -> Result<()> {
        let key = self.key(&format!("normalized/{}/{}.json", video.platform.as_str(), video.id));
        self.put_bytes(&key, serde_json::to_vec_pretty(video)?).await
    }

    async fn put_media(&self, artifact: &MediaArtifact) -> Result<()> {
        // yt-dlp положил в staging один или несколько файлов <id>* — грузим все
        // (для оконных скачиваний их несколько), затем чистим локальные копии.
        let id = &artifact.video.id;
        let mut uploaded = 0u32;
        let mut rd = tokio::fs::read_dir(&self.staging).await?;
        while let Ok(Some(entry)) = rd.next_entry().await {
            let name = entry.file_name();
            let name = name.to_string_lossy().into_owned();
            let is_partial =
                name.ends_with(".part") || name.ends_with(".tmp") || name.ends_with(".ytdl");
            if !name.starts_with(id.as_str()) || is_partial {
                continue;
            }
            let key = self.key(&format!("raw/videos/{name}"));
            self.put_file(&key, &entry.path()).await?;
            let _ = tokio::fs::remove_file(entry.path()).await;
            uploaded += 1;
        }
        debug!(%id, uploaded, "s3 media uploaded");
        Ok(())
    }

    fn staging_dir(&self) -> PathBuf {
        self.staging.clone()
    }

    fn discovery_log(&self, run_id: &str) -> PathBuf {
        self.staging.join("discovery").join(format!("{run_id}.jsonl"))
    }
}
