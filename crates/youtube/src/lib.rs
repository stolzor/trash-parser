//! YouTube discovery (Stage 0).
//!
//! Нативный InnerTube-клиент (reqwest) для query-поиска и аплоадов канала;
//! hashtag/trending — через `yt-dlp --flat-playlist`. Если нативный путь падает
//! (изменилась раскладка InnerTube), автоматически откатываемся на yt-dlp —
//! поэтому нативный discovery безопасно держать дефолтным.

mod innertube;

use detox_parser_core::error::Result;
use detox_parser_core::traits::Discoverer;
use detox_parser_core::types::*;
use detox_parser_ytdlp::{now_unix, YtDlp};
use innertube::{InnerTube, VideoHit};
use tracing::warn;

const DEFAULT_TARGET: usize = 40;

/// Канонический watch-URL по id.
pub fn watch_url(id: &str) -> String {
    format!("https://www.youtube.com/watch?v={id}")
}

pub struct YoutubeDiscoverer {
    it: InnerTube,
    yt: YtDlp,
}

impl YoutubeDiscoverer {
    pub fn new(yt: YtDlp) -> Self {
        Self { it: InnerTube::new(), yt }
    }

    /// Спек для yt-dlp fallback по типу источника.
    fn fallback_spec(seed: &Seed, n: usize) -> String {
        match seed.kind {
            SeedKind::Query => format!("ytsearch{n}:{}", seed.value),
            SeedKind::Hashtag => format!("ytsearch{n}:#{}", seed.value),
            SeedKind::Channel => {
                if seed.value.starts_with("http") {
                    seed.value.clone()
                } else if seed.value.starts_with('@') || seed.value.starts_with("UC") {
                    format!("https://www.youtube.com/{}/videos", seed.value)
                } else {
                    format!("https://www.youtube.com/@{}/videos", seed.value)
                }
            }
            SeedKind::Trending => format!("ytsearch{n}:trending {}", seed.value),
        }
    }

    /// yt-dlp flat-playlist → список VideoHit.
    async fn fallback(&self, seed: &Seed, n: usize) -> Result<Vec<VideoHit>> {
        let entries = self.yt.flat_playlist(&Self::fallback_spec(seed, n), n).await?;
        Ok(entries.into_iter().map(|e| VideoHit { id: e.id }).collect())
    }
}

#[async_trait::async_trait]
impl Discoverer for YoutubeDiscoverer {
    fn platform(&self) -> Platform {
        Platform::Youtube
    }

    async fn discover(&self, seed: &Seed) -> Result<Vec<Candidate>> {
        let n = seed.target.unwrap_or(DEFAULT_TARGET);

        // нативный InnerTube для query/channel; иначе сразу fallback
        let native = match seed.kind {
            SeedKind::Query => Some(self.it.search(&seed.value, n).await),
            SeedKind::Channel => Some(self.it.channel_videos(&seed.value, n).await),
            SeedKind::Hashtag | SeedKind::Trending => None,
        };

        let (hits, extractor) = match native {
            Some(Ok(h)) if !h.is_empty() => (h, "innertube"),
            Some(Ok(_)) => {
                warn!(value = %seed.value, "innertube вернул пусто → fallback yt-dlp");
                (self.fallback(seed, n).await?, "yt-dlp")
            }
            Some(Err(e)) => {
                warn!(value = %seed.value, error = %e, "innertube упал → fallback yt-dlp");
                (self.fallback(seed, n).await?, "yt-dlp")
            }
            None => (self.fallback(seed, n).await?, "yt-dlp"),
        };

        let ts = now_unix();
        Ok(hits
            .into_iter()
            .enumerate()
            .map(|(rank, h)| Candidate {
                video: VideoId::new(Platform::Youtube, h.id.clone()),
                url: watch_url(&h.id),
                provenance: Provenance {
                    source: format!("{:?}:{}", seed.kind, seed.value).to_lowercase(),
                    query: Some(seed.value.clone()),
                    rank: Some(rank as u32),
                    fetched_at: ts,
                    extractor: extractor.into(),
                    extractor_version: None,
                },
            })
            .collect())
    }
}
