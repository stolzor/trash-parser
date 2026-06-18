//! Конфиг пайплайна (десериализуется из config/seeds.toml).

use crate::types::{MediaOpts, Seed};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    /// Куда писать Bronze. Дефолт указывает на вход ml.
    #[serde(default = "default_out_root")]
    pub out_root: String,

    #[serde(default)]
    pub concurrency: Concurrency,

    #[serde(default)]
    pub media: MediaConfig,

    #[serde(default)]
    pub discovery: DiscoveryConfig,

    /// Квоты медиа по доменам (балансировка выборки short/long).
    #[serde(default)]
    pub quota: QuotaConfig,

    /// Настройки бэкенда yt-dlp (куки против бот-гейтинга YouTube).
    #[serde(default)]
    pub ytdlp: YtDlpConfig,

    /// Источники сбора (Stage 0).
    #[serde(default)]
    pub seeds: Vec<Seed>,
}

/// Квоты на скачивание медиа по доменам. `None` = без ограничения.
/// Гейтит дорогую медиа-ветку, чтобы обучающий набор был сбалансирован.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct QuotaConfig {
    pub short: Option<usize>,
    pub long: Option<usize>,
}

impl QuotaConfig {
    /// Квота для домена по его строковому имени ("short"/"long").
    pub fn for_domain(&self, domain: &str) -> Option<usize> {
        match domain {
            "short" => self.short,
            "long" => self.long,
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct YtDlpConfig {
    /// Брать куки из браузера, напр. "chrome"/"firefox"/"safari".
    /// YouTube всё чаще требует их для скачивания медиа ("confirm you're not a bot").
    pub cookies_from_browser: Option<String>,
    /// Либо путь к cookies.txt (Netscape-формат).
    pub cookies_file: Option<String>,
}

fn default_out_root() -> String {
    "../ml/data".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Concurrency {
    /// Одновременных задач метаданных на платформу.
    pub meta_workers: usize,
    /// Одновременных скачиваний медиа.
    pub media_workers: usize,
    /// Запросов в секунду на платформу (rate-limit).
    pub requests_per_sec: u32,
}

impl Default for Concurrency {
    fn default() -> Self {
        Self { meta_workers: 4, media_workers: 2, requests_per_sec: 2 }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MediaConfig {
    pub download: bool,
    pub format: String,
    pub max_filesize: Option<String>,
}

impl Default for MediaConfig {
    fn default() -> Self {
        Self {
            download: true,
            format: "worst[ext=mp4]/worst".into(),
            max_filesize: Some("100M".into()),
        }
    }
}

impl MediaConfig {
    pub fn to_opts(&self) -> MediaOpts {
        MediaOpts {
            format: self.format.clone(),
            max_filesize: self.max_filesize.clone(),
            ..MediaOpts::default()
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DiscoveryConfig {
    /// Сколько хопов расширения (0 = только seeds). Против survivorship bias:
    /// на каждом хопе тянем полные аплоады каналов уже собранных видео.
    pub max_hops: u32,
    /// Сколько аплоадов забирать с одного канала при расширении.
    pub expand_channel_videos: usize,
    /// Предел каналов, расширяемых за один хоп (ограничивает fan-out).
    pub max_channels_per_hop: usize,
    /// Лёгкий gate перед скачиванием медиа (Silver в ml — авторитетный).
    pub gate_before_media: bool,
    /// Минимальная длительность ролика (с) для прохода gate.
    pub min_duration_s: f64,
}

impl Default for DiscoveryConfig {
    fn default() -> Self {
        Self {
            max_hops: 0,
            expand_channel_videos: 30,
            max_channels_per_hop: 50,
            gate_before_media: true,
            min_duration_s: 3.0,
        }
    }
}
