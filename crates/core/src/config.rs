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

    /// Куда писать Bronze: локальная ФС или объектное хранилище.
    #[serde(default)]
    pub storage: StorageConfig,

    /// Состояние сбора (манифест) в PostgreSQL.
    #[serde(default)]
    pub manifest: ManifestConfig,

    /// Квоты медиа по доменам (балансировка выборки short/long).
    #[serde(default)]
    pub quota: QuotaConfig,

    /// Настройки бэкенда yt-dlp (куки против бот-гейтинга YouTube).
    #[serde(default)]
    pub ytdlp: YtDlpConfig,

    /// Пул прокси (файл со списком + ротация с cooldown).
    #[serde(default)]
    pub proxy: ProxyConfig,

    /// Cookie для нативного TikTok-клиента (msToken/ttwid против бот-стен).
    #[serde(default)]
    pub tiktok: TiktokConfig,

    /// Источники сбора (Stage 0).
    #[serde(default)]
    pub seeds: Vec<Seed>,
}

/// Cookie для нативного TikTok-discovery. `cookie` — готовая строка заголовка
/// (`msToken=…; ttwid=…; sessionid=…`), либо `cookie_file` — Netscape cookies.txt.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TiktokConfig {
    pub cookie: Option<String>,
    pub cookie_file: Option<String>,
}

/// Пул прокси. `file` — список (по одному URL на строку); пусто = без прокси.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ProxyConfig {
    pub file: Option<String>,
    /// Базовый cooldown (с) для прокси при сбое (растёт экспоненциально).
    pub cooldown_base_secs: u64,
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self { file: None, cooldown_base_secs: 30 }
    }
}

/// Манифест состояния (PostgreSQL). DSN из конфига или env DATABASE_URL.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ManifestConfig {
    pub url: String,
}

impl Default for ManifestConfig {
    fn default() -> Self {
        Self {
            url: std::env::var("DATABASE_URL").unwrap_or_else(|_| {
                "postgres://postgres:postgres@127.0.0.1:5432/detox".into()
            }),
        }
    }
}

/// Выбор бэкенда хранилища Bronze.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct StorageConfig {
    pub backend: StorageBackend,
    pub s3: S3Config,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StorageBackend {
    #[default]
    Fs,
    S3,
}

/// Параметры объектного хранилища. Креды — из env (AWS_ACCESS_KEY_ID/SECRET).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct S3Config {
    pub bucket: String,
    pub region: String,
    pub endpoint: Option<String>,
    pub prefix: String,
    pub path_style: bool,
    pub staging_dir: String,
}

impl Default for S3Config {
    fn default() -> Self {
        Self {
            bucket: String::new(),
            region: "us-east-1".into(),
            endpoint: None,
            prefix: String::new(),
            path_style: false,
            staging_dir: "./.staging".into(),
        }
    }
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
    // нейтральный локальный путь (для FS-режима и staging); данные от ml отделены
    "./data".into()
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
    /// Сколько задач захватывать за один claim. Маленький батч → несколько
    /// воркеров/процессов реально делят очередь (а не первый забирает всё).
    pub claim_batch: usize,
}

impl Default for Concurrency {
    fn default() -> Self {
        Self { meta_workers: 4, media_workers: 2, requests_per_sec: 2, claim_batch: 50 }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MediaConfig {
    pub download: bool,
    pub format: String,
    pub max_filesize: Option<String>,
    /// Сколько окон сэмплировать у длинных видео (0 = качать целиком).
    pub long_windows: usize,
    /// Длина окна (с) — синхронизировано с ml LONG_WINDOW_SECONDS.
    pub long_window_seconds: f64,
}

impl Default for MediaConfig {
    fn default() -> Self {
        Self {
            download: true,
            format: "worst[ext=mp4]/worst".into(),
            max_filesize: Some("100M".into()),
            long_windows: 4,
            long_window_seconds: 30.0,
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
