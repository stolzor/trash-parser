//! Bronze-хранилище на файловой системе. Раскладка совместима с ml/etl.md:
//! `raw/meta/<id>.json` — ровно то, что читает Silver-стадия ml.

use detox_parser_core::error::Result;
use detox_parser_core::traits::Sink;
use detox_parser_core::types::*;
use std::path::{Path, PathBuf};
use tokio::io::AsyncWriteExt;
use tracing::debug;

pub struct FsStore {
    root: PathBuf,
}

impl FsStore {
    /// Создаёт раскладку каталогов под `out_root`.
    pub async fn new(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        for sub in ["raw/meta", "raw/videos", "normalized/youtube", "normalized/tiktok", "discovery"] {
            tokio::fs::create_dir_all(root.join(sub)).await?;
        }
        Ok(Self { root })
    }

    async fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
        // пишем во временный и переименовываем — raw остаётся консистентным
        let tmp = path.with_extension("tmp");
        let mut f = tokio::fs::File::create(&tmp).await?;
        f.write_all(bytes).await?;
        f.sync_all().await?;
        tokio::fs::rename(&tmp, path).await?;
        Ok(())
    }
}

#[async_trait::async_trait]
impl Sink for FsStore {
    async fn put_raw(&self, extract: &RawExtract) -> Result<RawRef> {
        // flat raw/meta/<id>.json — контракт ml (Stage 1 Bronze, immutable)
        let path = self.root.join("raw/meta").join(format!("{}.json", extract.video.id));
        let bytes = serde_json::to_vec(&extract.raw)?;
        Self::write_atomic(&path, &bytes).await?;
        debug!(path = %path.display(), "wrote raw meta");
        Ok(RawRef { path: path.to_string_lossy().into_owned() })
    }

    async fn put_normalized(&self, video: &NormalizedVideo) -> Result<()> {
        let path = self
            .root
            .join("normalized")
            .join(video.platform.as_str())
            .join(format!("{}.json", video.id));
        let bytes = serde_json::to_vec_pretty(video)?;
        Self::write_atomic(&path, &bytes).await
    }

    async fn put_media(&self, artifact: &MediaArtifact) -> Result<()> {
        // yt-dlp уже скачал в staging_dir() (= финальное место); просто фиксируем.
        debug!(path = %artifact.path, bytes = artifact.bytes, "media present");
        Ok(())
    }

    fn staging_dir(&self) -> PathBuf {
        self.root.join("raw/videos")
    }

    fn discovery_log(&self, run_id: &str) -> PathBuf {
        self.root.join("discovery").join(format!("{run_id}.jsonl"))
    }
}
