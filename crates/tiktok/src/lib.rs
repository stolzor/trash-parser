//! TikTok discovery (Stage 0).
//!
//! v0: через `yt-dlp --flat-playlist`. Нативный TikTok web API требует
//! подписи запросов (X-Bogus/msToken) и крайне хрупок — поэтому отложен;
//! при необходимости добавляется как альтернативная реализация `Discoverer`.

use detox_parser_core::error::Result;
use detox_parser_core::traits::Discoverer;
use detox_parser_core::types::*;
use detox_parser_ytdlp::{now_unix, YtDlp};

const DEFAULT_TARGET: usize = 40;

pub struct TiktokDiscoverer {
    yt: YtDlp,
}

impl TiktokDiscoverer {
    pub fn new(yt: YtDlp) -> Self {
        Self { yt }
    }

    fn target_spec(seed: &Seed) -> String {
        match seed.kind {
            // у TikTok нет надёжного текстового поиска через yt-dlp → трактуем как тег
            SeedKind::Query | SeedKind::Hashtag | SeedKind::Trending => {
                let tag = seed.value.trim_start_matches('#');
                format!("https://www.tiktok.com/tag/{tag}")
            }
            SeedKind::Channel => {
                if seed.value.starts_with("http") {
                    seed.value.clone()
                } else {
                    let user = seed.value.trim_start_matches('@');
                    format!("https://www.tiktok.com/@{user}")
                }
            }
        }
    }
}

#[async_trait::async_trait]
impl Discoverer for TiktokDiscoverer {
    fn platform(&self) -> Platform {
        Platform::Tiktok
    }

    async fn discover(&self, seed: &Seed) -> Result<Vec<Candidate>> {
        let n = seed.target.unwrap_or(DEFAULT_TARGET);
        let target = Self::target_spec(seed);
        let entries = self.yt.flat_playlist(&target, n).await?;
        let ts = now_unix();
        Ok(entries
            .into_iter()
            .enumerate()
            .map(|(rank, e)| Candidate {
                // у TikTok канонический URL приходит из flat-playlist (id недостаточно)
                video: VideoId::new(Platform::Tiktok, e.id.clone()),
                url: e.url.clone(),
                provenance: Provenance {
                    source: format!("{:?}:{}", seed.kind, seed.value).to_lowercase(),
                    query: Some(seed.value.clone()),
                    rank: Some(rank as u32),
                    fetched_at: ts,
                    extractor: "yt-dlp".into(),
                    extractor_version: None,
                },
            })
            .collect())
    }
}
