//! TikTok discovery (Stage 0).
//!
//! Нативный best-effort путь (web.rs): GET страницы тега/пользователя → встроенный
//! JSON → список видео, без подписи запросов. Если страница не отдала items
//! (бот-стена / items грузятся подписанным XHR), откатываемся на yt-dlp —
//! как и для YouTube, поэтому нативный путь безопасно держать дефолтным.

pub mod web;

use detox_parser_core::error::Result;
use detox_parser_core::traits::Discoverer;
use detox_parser_core::types::*;
use detox_parser_core::ProxyPool;
use detox_parser_ytdlp::{now_unix, YtDlp};
use std::sync::Arc;
use tracing::warn;
pub use web::{TikTokItem, TikTokWeb};

const DEFAULT_TARGET: usize = 40;

/// Канонический URL видео (yt-dlp принимает `@_` как плейсхолдер автора).
fn video_url(username: Option<&str>, id: &str) -> String {
    format!("https://www.tiktok.com/@{}/video/{id}", username.unwrap_or("_"))
}

/// Построить строку заголовка `Cookie` из Netscape cookies.txt (формат yt-dlp),
/// взяв куки домена tiktok. `None`, если файл пуст/нечитаем/без tiktok-кук.
pub fn cookie_header_from_file(path: &str) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut pairs = Vec::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        // `#HttpOnly_` — реальные куки; прочие `#`-строки — комментарии
        let line = match line.strip_prefix("#HttpOnly_") {
            Some(rest) => rest,
            None if line.starts_with('#') => continue,
            None => line,
        };
        let f: Vec<&str> = line.split('\t').collect();
        if f.len() < 7 || !f[0].contains("tiktok") {
            continue;
        }
        pairs.push(format!("{}={}", f[5], f[6]));
    }
    if pairs.is_empty() {
        None
    } else {
        Some(pairs.join("; "))
    }
}

pub struct TiktokDiscoverer {
    web: TikTokWeb,
    yt: YtDlp,
}

impl TiktokDiscoverer {
    pub fn new(yt: YtDlp, proxy: Option<Arc<ProxyPool>>, cookie: Option<String>) -> Self {
        Self { web: TikTokWeb::new(proxy, cookie), yt }
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

#[cfg(test)]
mod tests {
    use super::cookie_header_from_file;

    #[test]
    fn parses_netscape_cookies_for_tiktok_only() {
        let path = std::env::temp_dir().join(format!("tt_cookies_{}.txt", std::process::id()));
        std::fs::write(
            &path,
            "# Netscape HTTP Cookie File\n\
             .tiktok.com\tTRUE\t/\tTRUE\t0\tmsToken\tABC123\n\
             #HttpOnly_.tiktok.com\tTRUE\t/\tTRUE\t0\tttwid\tXYZ\n\
             .youtube.com\tTRUE\t/\tTRUE\t0\tPREF\tignored\n",
        )
        .unwrap();
        let header = cookie_header_from_file(path.to_str().unwrap()).unwrap();
        assert!(header.contains("msToken=ABC123"));
        assert!(header.contains("ttwid=XYZ"), "должен брать и #HttpOnly_ куки");
        assert!(!header.contains("PREF"), "куки чужих доменов отброшены");
        let _ = std::fs::remove_file(&path);
    }
}
