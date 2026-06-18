//! TikTok discovery (Stage 0).
//!
//! Нативный best-effort путь (web.rs): GET страницы тега/пользователя → встроенный
//! JSON → список видео, без подписи запросов. Если страница не отдала items
//! (бот-стена / items грузятся подписанным XHR), откатываемся на yt-dlp —
//! как и для YouTube, поэтому нативный путь безопасно держать дефолтным.

mod web;

use detox_parser_core::error::Result;
use detox_parser_core::traits::Discoverer;
use detox_parser_core::types::*;
use detox_parser_core::ProxyPool;
use detox_parser_ytdlp::{now_unix, YtDlp};
use std::sync::Arc;
use tracing::warn;
use web::{TikTokItem, TikTokWeb};

const DEFAULT_TARGET: usize = 40;

/// Канонический URL видео (yt-dlp принимает `@_` как плейсхолдер автора).
fn video_url(username: Option<&str>, id: &str) -> String {
    format!("https://www.tiktok.com/@{}/video/{id}", username.unwrap_or("_"))
}

pub struct TiktokDiscoverer {
    web: TikTokWeb,
    yt: YtDlp,
}

impl TiktokDiscoverer {
    pub fn new(yt: YtDlp, proxy: Option<Arc<ProxyPool>>) -> Self {
        Self { web: TikTokWeb::new(proxy), yt }
    }

    /// URL страницы для нативного парсинга.
    fn page_url(seed: &Seed) -> String {
        match seed.kind {
            SeedKind::Query | SeedKind::Hashtag | SeedKind::Trending => {
                format!("https://www.tiktok.com/tag/{}", seed.value.trim_start_matches('#'))
            }
            SeedKind::Channel => {
                if seed.value.starts_with("http") {
                    seed.value.clone()
                } else {
                    format!("https://www.tiktok.com/@{}", seed.value.trim_start_matches('@'))
                }
            }
        }
    }

    /// Спек для yt-dlp fallback.
    fn fallback_spec(seed: &Seed) -> String {
        match seed.kind {
            SeedKind::Query | SeedKind::Hashtag | SeedKind::Trending => {
                let tag = seed.value.trim_start_matches('#');
                format!("https://www.tiktok.com/tag/{tag}")
            }
            SeedKind::Channel => {
                if seed.value.starts_with("http") {
                    seed.value.clone()
                } else {
                    format!("https://www.tiktok.com/@{}", seed.value.trim_start_matches('@'))
                }
            }
        }
    }

    async fn fallback(&self, seed: &Seed, n: usize) -> Result<Vec<TikTokItem>> {
        let entries = self.yt.flat_playlist(&Self::fallback_spec(seed), n).await?;
        Ok(entries
            .into_iter()
            .map(|e| TikTokItem { id: e.id, username: None })
            .collect())
    }
}

#[async_trait::async_trait]
impl Discoverer for TiktokDiscoverer {
    fn platform(&self) -> Platform {
        Platform::Tiktok
    }

    async fn discover(&self, seed: &Seed) -> Result<Vec<Candidate>> {
        let n = seed.target.unwrap_or(DEFAULT_TARGET);

        // нативный путь, затем fallback на yt-dlp при ошибке/пустоте
        let (items, extractor) = match self.web.discover_url(&Self::page_url(seed), n).await {
            Ok(items) if !items.is_empty() => (items, "tiktok-web"),
            Ok(_) => {
                warn!(value = %seed.value, "tiktok-web вернул пусто → fallback yt-dlp");
                (self.fallback(seed, n).await?, "yt-dlp")
            }
            Err(e) => {
                warn!(value = %seed.value, error = %e, "tiktok-web упал → fallback yt-dlp");
                (self.fallback(seed, n).await?, "yt-dlp")
            }
        };

        let ts = now_unix();
        Ok(items
            .into_iter()
            .enumerate()
            .map(|(rank, it)| {
                let url = video_url(it.username.as_deref(), &it.id);
                Candidate {
                    video: VideoId::new(Platform::Tiktok, it.id),
                    url,
                    provenance: Provenance {
                        source: format!("{:?}:{}", seed.kind, seed.value).to_lowercase(),
                        query: Some(seed.value.clone()),
                        rank: Some(rank as u32),
                        fetched_at: ts,
                        extractor: extractor.into(),
                        extractor_version: None,
                    },
                }
            })
            .collect())
    }
}
