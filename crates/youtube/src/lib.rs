//! YouTube discovery (Stage 0).
//!
//! v0: реализует `Discoverer` через `yt-dlp --flat-playlist` — надёжно и
//! работает сразу. Нативный InnerTube-клиент на `reqwest` можно добавить позже
//! как альтернативную реализацию того же трейта, не трогая pipeline.

use detox_parser_core::error::Result;
use detox_parser_core::traits::Discoverer;
use detox_parser_core::types::*;
use detox_parser_ytdlp::{now_unix, YtDlp};

const DEFAULT_TARGET: usize = 40;

/// Канонический watch-URL по id.
pub fn watch_url(id: &str) -> String {
    format!("https://www.youtube.com/watch?v={id}")
}

pub struct YoutubeDiscoverer {
    yt: YtDlp,
}

impl YoutubeDiscoverer {
    pub fn new(yt: YtDlp) -> Self {
        Self { yt }
    }

    /// Спек для yt-dlp по типу источника.
    fn target_spec(seed: &Seed, n: usize) -> String {
        match seed.kind {
            SeedKind::Query => format!("ytsearch{n}:{}", seed.value),
            // тег ищем как обычный запрос с # — YT-страница хэштега нестабильна
            SeedKind::Hashtag => format!("ytsearch{n}:#{}", seed.value),
            // полные аплоады канала — для низко-вовлечённых примеров (анти-bias)
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
}

#[async_trait::async_trait]
impl Discoverer for YoutubeDiscoverer {
    fn platform(&self) -> Platform {
        Platform::Youtube
    }

    async fn discover(&self, seed: &Seed) -> Result<Vec<Candidate>> {
        let n = seed.target.unwrap_or(DEFAULT_TARGET);
        let target = Self::target_spec(seed, n);
        let entries = self.yt.flat_playlist(&target, n).await?;
        let ts = now_unix();
        Ok(entries
            .into_iter()
            .enumerate()
            .map(|(rank, e)| Candidate {
                video: VideoId::new(Platform::Youtube, e.id.clone()),
                url: watch_url(&e.id),
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
