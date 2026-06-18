//! Доменные типы — общий язык всех крейтов.

use serde::{Deserialize, Serialize};
use std::fmt;

/// Версия нормализованной схемы. Бампать при несовместимых изменениях
/// `NormalizedVideo`, чтобы потребители не читали старое молча.
pub const SCHEMA_VERSION: u32 = 1;

/// Порог домена в секундах (синхронизировано с ml/dopamine/config.py).
pub const SHORT_MAX_SECONDS: f64 = 90.0;

// --- Платформа / идентификатор --------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Platform {
    Youtube,
    Tiktok,
}

impl Platform {
    pub fn as_str(&self) -> &'static str {
        match self {
            Platform::Youtube => "youtube",
            Platform::Tiktok => "tiktok",
        }
    }
}

impl fmt::Display for Platform {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Идентификатор видео внутри платформы (натуральный ключ вместе с `Platform`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct VideoId {
    pub platform: Platform,
    pub id: String,
}

impl VideoId {
    pub fn new(platform: Platform, id: impl Into<String>) -> Self {
        Self { platform, id: id.into() }
    }
}

impl fmt::Display for VideoId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.platform, self.id)
    }
}

/// Домен модели (ml роутит по длительности).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Domain {
    Short,
    Long,
}

impl Domain {
    pub fn for_duration(duration_s: f64) -> Self {
        if duration_s <= SHORT_MAX_SECONDS {
            Domain::Short
        } else {
            Domain::Long
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Domain::Short => "short",
            Domain::Long => "long",
        }
    }
}

// --- Stage 0: discovery -----------------------------------------------------

/// Один источник сбора (строка из seeds.toml).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Seed {
    pub platform: Platform,
    pub kind: SeedKind,
    /// Значение: текст запроса, хэштег, id канала/пользователя.
    pub value: String,
    /// Подсказка домена для балансировки выборки (не авторитетна).
    #[serde(default)]
    pub domain_hint: Option<Domain>,
    /// Сколько кандидатов хотим получить из этого источника.
    #[serde(default)]
    pub target: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SeedKind {
    /// Полнотекстовый поиск.
    Query,
    /// Хэштег / челлендж (TikTok) или тег (YouTube).
    Hashtag,
    /// Полные аплоады канала/пользователя — для низко-вовлечённых примеров.
    Channel,
    /// Тренды/популярное по региону.
    Trending,
}

/// Кандидат, найденный на Stage 0, с провенансом.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Candidate {
    pub video: VideoId,
    pub url: String,
    pub provenance: Provenance,
}

/// Откуда и когда пришли данные (etl.md: провенанс-колонки против смещений).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Provenance {
    /// Тип+значение источника, напр. "hashtag:running".
    pub source: String,
    pub query: Option<String>,
    /// Позиция в выдаче (для анализа survivorship bias).
    pub rank: Option<u32>,
    /// Unix-время фетча (передаётся снаружи — в ядре нет часов).
    pub fetched_at: i64,
    /// Чем извлекали ("yt-dlp", "innertube").
    pub extractor: String,
    pub extractor_version: Option<String>,
}

// --- Stage 1: extraction ----------------------------------------------------

/// Результат экстракции: сырой ответ (immutable bronze) + нормализованная запись.
#[derive(Debug, Clone)]
pub struct RawExtract {
    pub video: VideoId,
    /// Полный JSON экстрактора как есть — ничего не теряем.
    pub raw: serde_json::Value,
    pub normalized: NormalizedVideo,
}

/// Указатель на сохранённый сырой JSON.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawRef {
    pub path: String,
}

/// Кросс-платформенная нормализованная запись. Поля-нужные-ml + аналитика.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NormalizedVideo {
    pub schema_version: u32,
    pub platform: Platform,
    pub id: String,
    pub url: String,

    pub title: Option<String>,
    pub description: Option<String>,
    pub channel: Channel,

    /// Длительность в секундах.
    pub duration_s: Option<f64>,
    pub domain: Option<Domain>,

    // --- сигналы вовлечённости (контракт ml) ---
    pub view_count: Option<u64>,
    pub like_count: Option<u64>,
    pub comment_count: Option<u64>,
    pub share_count: Option<u64>,

    /// Формат YYYYMMDD (ml парсит upload_date именно так).
    pub upload_date: Option<String>,
    /// Unix-время публикации, если известно.
    pub timestamp: Option<i64>,

    #[serde(default)]
    pub categories: Vec<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub hashtags: Vec<String>,
    pub language: Option<String>,

    #[serde(default)]
    pub thumbnails: Vec<Thumbnail>,
    pub format: Option<MediaFormat>,
    #[serde(default)]
    pub captions: Vec<String>,

    /// YouTube «most replayed» (нормализованная кривая интенсивности).
    pub heatmap: Option<serde_json::Value>,
    pub chapters: Option<serde_json::Value>,
    /// Инфо о музыке (TikTok).
    pub music: Option<serde_json::Value>,

    pub provenance: Provenance,
    /// Путь к сырому JSON (заполняется store).
    pub raw_path: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Channel {
    pub id: Option<String>,
    pub name: Option<String>,
    pub url: Option<String>,
    pub follower_count: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Thumbnail {
    pub url: String,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

/// Дешёвые format-фичи без скачивания (etl.md, A0).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MediaFormat {
    pub fps: Option<f64>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub aspect_ratio: Option<f64>,
    pub vcodec: Option<String>,
    pub acodec: Option<String>,
    /// Суммарный битрейт (kbps).
    pub tbr: Option<f64>,
    pub filesize: Option<u64>,
}

// --- Stage 1b: media --------------------------------------------------------

/// Параметры скачивания медиа.
#[derive(Debug, Clone)]
pub struct MediaOpts {
    /// yt-dlp -f, напр. "worst[ext=mp4]/worst".
    pub format: String,
    /// Кап размера, напр. "100M".
    pub max_filesize: Option<String>,
    pub socket_timeout_s: u64,
    pub retries: u32,
}

impl Default for MediaOpts {
    fn default() -> Self {
        Self {
            format: "worst[ext=mp4]/worst".into(),
            max_filesize: Some("100M".into()),
            socket_timeout_s: 30,
            retries: 2,
        }
    }
}

#[derive(Debug, Clone)]
pub struct MediaArtifact {
    pub video: VideoId,
    pub path: String,
    pub bytes: u64,
}
