//! Трейты-границы пайплайна. Адаптеры (youtube/tiktok/ytdlp/store) реализуют их;
//! pipeline зависит только от этих трейтов, не от конкретных бэкендов.

use crate::error::Result;
use crate::types::*;
use async_trait::async_trait;
use std::path::PathBuf;

/// Stage 0 — поиск кандидатов по источнику.
#[async_trait]
pub trait Discoverer: Send + Sync {
    fn platform(&self) -> Platform;
    async fn discover(&self, seed: &Seed) -> Result<Vec<Candidate>>;
}

/// Stage 1 — извлечение полных метаданных. `url` берётся из discovery
/// (для TikTok одного id недостаточно для канонического URL).
#[async_trait]
pub trait Extractor: Send + Sync {
    fn platform(&self) -> Platform;
    async fn metadata(&self, video: &VideoId, url: &str) -> Result<RawExtract>;
}

/// Stage 1b — скачивание медиафайла.
#[async_trait]
pub trait MediaFetcher: Send + Sync {
    async fn fetch(&self, video: &VideoId, url: &str, opts: &MediaOpts)
        -> Result<MediaArtifact>;
}

/// Bronze-хранилище. Raw immutable; нормализованное и медиа — рядом.
/// Реализации: локальная ФС (FsStore) и объектное хранилище (S3Store).
#[async_trait]
pub trait Sink: Send + Sync {
    async fn put_raw(&self, extract: &RawExtract) -> Result<RawRef>;
    async fn put_normalized(&self, video: &NormalizedVideo) -> Result<()>;
    /// Принять медиа. `artifact.path` — локальный файл, записанный yt-dlp в
    /// `staging_dir()`; реализация может его загрузить/переместить.
    async fn put_media(&self, artifact: &MediaArtifact) -> Result<()>;

    /// Локальный каталог, куда yt-dlp пишет медиа до приёма sink-ом.
    /// Для ФС это и есть финальное место; для S3 — временный staging.
    fn staging_dir(&self) -> PathBuf;

    /// Локальный путь лога провенанса discovery для прогона.
    fn discovery_log(&self, run_id: &str) -> PathBuf;
}
