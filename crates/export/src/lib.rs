//! Экспорт нормализованных записей в один parquet-файл для аналитики.
//!
//! Это аналитический артефакт поверх Bronze, НЕ замена Silver в ml. Плоская
//! схема (скаляры + списки строк) удобна для DuckDB/Polars/pandas; вложенные
//! json-поля (heatmap/chapters/music) сворачиваются в флаги/счётчики.

use arrow::datatypes::FieldRef;
use detox_parser_core::error::{Error, Result};
use detox_parser_core::types::NormalizedVideo;
use parquet::arrow::ArrowWriter;
use parquet::basic::Compression;
use parquet::file::properties::WriterProperties;
use serde::{Deserialize, Serialize};
use serde_arrow::schema::{SchemaLike, TracingOptions};
use std::path::Path;
use tracing::debug;

/// Плоская строка экспорта — все поля arrow-дружелюбны.
#[derive(Debug, Serialize, Deserialize)]
pub struct ExportRow {
    pub platform: String,
    pub id: String,
    pub url: String,
    pub title: Option<String>,

    pub channel_id: Option<String>,
    pub channel_name: Option<String>,
    pub channel_follower_count: Option<u64>,

    pub duration_s: Option<f64>,
    pub domain: Option<String>,

    pub view_count: Option<u64>,
    pub like_count: Option<u64>,
    pub comment_count: Option<u64>,
    pub share_count: Option<u64>,

    pub upload_date: Option<String>,
    pub timestamp: Option<i64>,

    pub categories: Vec<String>,
    pub tags: Vec<String>,
    pub hashtags: Vec<String>,
    pub language: Option<String>,

    pub fps: Option<f64>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub aspect_ratio: Option<f64>,
    pub vcodec: Option<String>,
    pub tbr: Option<f64>,

    // вложенные json-поля → дешёвые признаки наличия/объёма
    pub has_heatmap: bool,
    pub caption_count: u32,

    pub source: String,
    pub query: Option<String>,
    pub extractor: String,
    pub fetched_at: i64,
    pub raw_path: Option<String>,
}

impl From<&NormalizedVideo> for ExportRow {
    fn from(v: &NormalizedVideo) -> Self {
        let f = v.format.clone().unwrap_or_default();
        ExportRow {
            platform: v.platform.as_str().into(),
            id: v.id.clone(),
            url: v.url.clone(),
            title: v.title.clone(),
            channel_id: v.channel.id.clone(),
            channel_name: v.channel.name.clone(),
            channel_follower_count: v.channel.follower_count,
            duration_s: v.duration_s,
            domain: v.domain.map(|d| d.as_str().to_string()),
            view_count: v.view_count,
            like_count: v.like_count,
            comment_count: v.comment_count,
            share_count: v.share_count,
            upload_date: v.upload_date.clone(),
            timestamp: v.timestamp,
            categories: v.categories.clone(),
            tags: v.tags.clone(),
            hashtags: v.hashtags.clone(),
            language: v.language.clone(),
            fps: f.fps,
            width: f.width,
            height: f.height,
            aspect_ratio: f.aspect_ratio,
            vcodec: f.vcodec,
            tbr: f.tbr,
            has_heatmap: v.heatmap.is_some(),
            caption_count: v.captions.len() as u32,
            source: v.provenance.source.clone(),
            query: v.provenance.query.clone(),
            extractor: v.provenance.extractor.clone(),
            fetched_at: v.provenance.fetched_at,
            raw_path: v.raw_path.clone(),
        }
    }
}

/// Прочитать все `normalized/<platform>/*.json` под `out_root`.
fn read_rows(out_root: &Path) -> Result<Vec<ExportRow>> {
    let mut rows = Vec::new();
    let norm_dir = out_root.join("normalized");
    let platform_dirs = match std::fs::read_dir(&norm_dir) {
        Ok(d) => d,
        Err(_) => return Ok(rows), // ещё ничего не собрано
    };
    for pd in platform_dirs.flatten() {
        if !pd.path().is_dir() {
            continue;
        }
        for entry in std::fs::read_dir(pd.path())?.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let bytes = std::fs::read(&path)?;
            match serde_json::from_slice::<NormalizedVideo>(&bytes) {
                Ok(v) => rows.push(ExportRow::from(&v)),
                Err(e) => debug!(path = %path.display(), error = %e, "skip unparsable normalized"),
            }
        }
    }
    Ok(rows)
}

/// Собрать нормализованные записи под `out_root` в parquet по пути `out_path`.
/// Возвращает число записанных строк.
pub fn export_parquet(out_root: &Path, out_path: &Path) -> Result<usize> {
    let rows = read_rows(out_root)?;
    if rows.is_empty() {
        return Ok(0);
    }
    if let Some(parent) = out_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let fields = Vec::<FieldRef>::from_type::<ExportRow>(TracingOptions::default())
        .map_err(|e| Error::Storage(format!("arrow schema: {e}")))?;
    let batch = serde_arrow::to_record_batch(&fields, &rows)
        .map_err(|e| Error::Storage(format!("arrow batch: {e}")))?;

    let file = std::fs::File::create(out_path)?;
    let props = WriterProperties::builder()
        .set_compression(Compression::SNAPPY)
        .build();
    let mut writer = ArrowWriter::try_new(file, batch.schema(), Some(props))
        .map_err(|e| Error::Storage(format!("parquet writer: {e}")))?;
    writer.write(&batch).map_err(|e| Error::Storage(format!("parquet write: {e}")))?;
    writer.close().map_err(|e| Error::Storage(format!("parquet close: {e}")))?;

    Ok(rows.len())
}
