//! Async-обёртка над `yt-dlp` (subprocess). Реализует `Extractor` и
//! `MediaFetcher` ядра. Брутальную reverse-engineering часть (cipher,
//! n-throttling, протухающие URL) держит yt-dlp, мы лишь оркестрируем.

mod normalize;

use detox_parser_core::error::{Error, Result};
use detox_parser_core::traits::{Extractor, MediaFetcher};
use detox_parser_core::types::*;
use detox_parser_core::ProxyPool;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use tokio::process::Command;
use tracing::debug;

pub use normalize::normalize;

/// Куки для обхода бот-гейтинга. Применяются ко всем вызовам yt-dlp.
#[derive(Clone, Default)]
pub struct CookieOpts {
    pub from_browser: Option<String>,
    pub file: Option<String>,
}

/// Клиент yt-dlp. Платформа фиксируется при создании (для `Extractor::platform`).
#[derive(Clone)]
pub struct YtDlp {
    bin: String,
    platform: Platform,
    /// Куда складывать медиа (шаблон дополняется в pipeline/store).
    media_dir: PathBuf,
    /// Префикс-аргументы с куками (пусто, если не заданы).
    cookie_args: Vec<String>,
    /// Пул прокси для discovery/метаданных (лёгкие запросы — сюда ставят чистый
    /// residential). Ротация на каждый вызов yt-dlp.
    meta_proxy: Option<Arc<ProxyPool>>,
    /// Пул прокси для скачивания медиа (гигабайты). Может быть другим пулом или
    /// `None` (скачивание напрямую, мимо дорогого пула).
    media_proxy: Option<Arc<ProxyPool>>,
}

impl YtDlp {
    pub fn new(platform: Platform, media_dir: impl Into<PathBuf>) -> Self {
        Self::build(platform, media_dir, &CookieOpts::default(), None, None)
    }

    pub fn with_cookies(
        platform: Platform,
        media_dir: impl Into<PathBuf>,
        cookies: &CookieOpts,
    ) -> Self {
        Self::build(platform, media_dir, cookies, None, None)
    }

    /// Полный конструктор: куки + раздельные пулы прокси под метаданные и медиа.
    /// `meta_proxy` обслуживает `-J`/`--flat-playlist` (discovery+метаданные),
    /// `media_proxy` — скачивание (может быть тем же пулом, другим или `None`).
    pub fn build(
        platform: Platform,
        media_dir: impl Into<PathBuf>,
        cookies: &CookieOpts,
        meta_proxy: Option<Arc<ProxyPool>>,
        media_proxy: Option<Arc<ProxyPool>>,
    ) -> Self {
        let mut cookie_args = Vec::new();
        if let Some(browser) = &cookies.from_browser {
            cookie_args.push("--cookies-from-browser".into());
            cookie_args.push(browser.clone());
        }
        if let Some(file) = &cookies.file {
            cookie_args.push("--cookies".into());
            cookie_args.push(file.clone());
        }
        Self {
            bin: std::env::var("YTDLP_BIN").unwrap_or_else(|_| "yt-dlp".into()),
            platform,
            media_dir: media_dir.into(),
            cookie_args,
            meta_proxy,
            media_proxy,
        }
    }

    /// Классификация stderr yt-dlp в наш тип ошибок (ретраить или нет).
    fn classify(&self, stderr: &str) -> Error {
        let s = stderr.to_lowercase();
        if s.contains("http error 429") || s.contains("too many requests") {
            Error::RateLimited(stderr.trim().to_owned())
        } else if s.contains("confirm you") || s.contains("sign in") {
            // Бот-гейтинг завязан на репутацию IP. Если есть пул прокси — ретрай
            // попадёт на другой узел, поэтому это временная ошибка: помечаем
            // текущий IP плохим (Transient → cooldown+ротация в пуле) и пробуем
            // заново. Без пула лечится только куками → терминальная Tool с
            // подсказкой (ретрай с того же IP бесполезен).
            if self.has_proxy() {
                Error::Transient(format!("бот-гейт, ротирую IP: {}", stderr.trim()))
            } else {
                Error::Tool(format!(
                    "{} [подсказка: задай residential [proxy] или [ytdlp] cookies_file]",
                    stderr.trim()
                ))
            }
        } else if s.contains("http error 403") || s.contains("forbidden") {
            // часто троттлинг/протухший URL — ретрай с backoff может помочь
            Error::Transient(stderr.trim().to_owned())
        } else if s.contains("private")
            || s.contains("removed")
            || s.contains("unavailable")
            || s.contains("age")
            || s.contains("not available")
        {
            Error::Unavailable(stderr.trim().to_owned())
        } else if s.contains("timed out") || s.contains("temporary") || s.contains("503") {
            Error::Transient(stderr.trim().to_owned())
        } else {
            Error::Tool(stderr.trim().to_owned())
        }
    }

    /// Есть ли хоть какой-то пул прокси (для решения о ретраибельности бот-гейта).
    fn has_proxy(&self) -> bool {
        self.meta_proxy.is_some() || self.media_proxy.is_some()
    }

    /// Запустить yt-dlp через заданный пул прокси (`pool` — метаданные или медиа).
    async fn run(
        &self,
        args: &[&str],
        pool: Option<&Arc<ProxyPool>>,
    ) -> Result<std::process::Output> {
        let now = now_unix();
        let proxy = pool.and_then(|p| p.acquire(now));

        let mut full: Vec<&str> = self.cookie_args.iter().map(String::as_str).collect();
        if let Some(px) = &proxy {
            full.push("--proxy");
            full.push(px);
        }
        full.extend_from_slice(args);
        debug!(?full, "yt-dlp");

        let result = self.run_inner(&full).await;

        // отчёт прокси: сбой засчитываем только для сетевых причин (429/transient,
        // включая бот-гейт — он теперь Transient при наличии пула → узел в cooldown)
        if let (Some(pool), Some(px)) = (pool, &proxy) {
            let ok = !matches!(&result, Err(Error::RateLimited(_)) | Err(Error::Transient(_)));
            pool.report(px, ok, now);
        }
        result
    }

    async fn run_inner(&self, full: &[&str]) -> Result<std::process::Output> {
        let out = Command::new(&self.bin)
            .args(full)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await
            .map_err(|e| {
                Error::Tool(format!("не удалось запустить '{}': {e} (установлен ли yt-dlp?)", self.bin))
            })?;
        if !out.status.success() {
            return Err(self.classify(&String::from_utf8_lossy(&out.stderr)));
        }
        Ok(out)
    }
}

#[async_trait::async_trait]
impl Extractor for YtDlp {
    fn platform(&self) -> Platform {
        self.platform
    }

    async fn metadata(&self, video: &VideoId, url: &str) -> Result<RawExtract> {
        let out = self
            .run(&["-J", "--no-warnings", "--no-playlist", url], self.meta_proxy.as_ref())
            .await?;
        let raw: serde_json::Value = serde_json::from_slice(&out.stdout)
            .map_err(|e| Error::Parse(format!("yt-dlp -J не распарсился: {e}")))?;

        let provenance = Provenance {
            source: format!("extract:{}", self.platform),
            query: None,
            rank: None,
            fetched_at: now_unix(),
            extractor: "yt-dlp".into(),
            extractor_version: None,
        };
        let normalized = normalize(self.platform, &video.id, url, &raw, provenance);
        Ok(RawExtract { video: video.clone(), raw, normalized })
    }
}

#[async_trait::async_trait]
impl MediaFetcher for YtDlp {
    async fn fetch(&self, video: &VideoId, url: &str, opts: &MediaOpts) -> Result<MediaArtifact> {
        let windowed = !opts.sections.is_empty();
        // для окон шаблон включает start, чтобы файлы не перезаписывались
        let out_tmpl = if windowed {
            self.media_dir.join(format!("{}.win_%(section_start)s.%(ext)s", video.id))
        } else {
            self.media_dir.join(format!("{}.%(ext)s", video.id))
        }
        .to_string_lossy()
        .into_owned();

        // owned-строки: section-аргументы строятся динамически
        let mut args: Vec<String> = vec![
            "--no-warnings".into(),
            "--no-playlist".into(),
            "-f".into(),
            opts.format.clone(),
            "-o".into(),
            out_tmpl,
            "--socket-timeout".into(),
            opts.socket_timeout_s.to_string(),
            "--retries".into(),
            opts.retries.to_string(),
            // печатаем путь(и) итоговых файлов на stdout
            "--print".into(),
            "after_move:filepath".into(),
        ];
        for (start, end) in &opts.sections {
            args.push("--download-sections".into());
            args.push(format!("*{start}-{end}"));
        }
        if let Some(cap) = &opts.max_filesize {
            args.push("--max-filesize".into());
            args.push(cap.clone());
        }
        args.push(url.to_string());

        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let out = self.run(&arg_refs, self.media_proxy.as_ref()).await?;

        // при окнах yt-dlp печатает по строке на файл
        let stdout = String::from_utf8_lossy(&out.stdout);
        let paths: Vec<&str> = stdout.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
        if paths.is_empty() {
            return Err(Error::Tool(
                "yt-dlp не вернул путь к файлу (возможно, превышен max-filesize)".into(),
            ));
        }
        let mut total = 0u64;
        for p in &paths {
            total += tokio::fs::metadata(p).await.map(|m| m.len()).unwrap_or(0);
        }
        Ok(MediaArtifact { video: video.clone(), path: paths[0].to_owned(), bytes: total })
    }
}

/// Запись из flat-playlist (discovery): id известен, метадата ещё нет.
#[derive(Debug, Clone)]
pub struct FlatEntry {
    pub id: String,
    pub url: String,
    pub title: Option<String>,
}

impl YtDlp {
    /// Discovery-примитив: развернуть search/канал/плейлист в список id без
    /// скачивания метаданных каждого (`--flat-playlist`). `target` — это
    /// поисковый спек ("ytsearch40:...") или URL канала/хэштега.
    pub async fn flat_playlist(&self, target: &str, limit: usize) -> Result<Vec<FlatEntry>> {
        let lim = limit.to_string();
        let out = self
            .run(
                &["--flat-playlist", "--no-warnings", "-J", "--playlist-end", &lim, target],
                self.meta_proxy.as_ref(),
            )
            .await?;
        let root: serde_json::Value = serde_json::from_slice(&out.stdout)
            .map_err(|e| Error::Parse(format!("flat-playlist JSON: {e}")))?;

        let entries = root
            .get("entries")
            .and_then(|e| e.as_array())
            .ok_or_else(|| Error::Parse("flat-playlist без entries".into()))?;

        Ok(entries
            .iter()
            .filter_map(|e| {
                let id = e.get("id")?.as_str()?.to_owned();
                let url = e
                    .get("url")
                    .and_then(|u| u.as_str())
                    .map(str::to_owned)
                    .unwrap_or_else(|| id.clone());
                Some(FlatEntry {
                    id,
                    url,
                    title: e.get("title").and_then(|t| t.as_str()).map(str::to_owned),
                })
            })
            .collect())
    }
}

impl YtDlp {
    /// Экспортировать куки браузера в Netscape-файл через yt-dlp (нативные
    /// reqwest-клиенты сами куки браузера читать не умеют). Best-effort:
    /// `--playlist-items 0` не тянет видео, но куки уже загружаются и пишутся.
    pub async fn export_cookies(
        &self,
        browser: &str,
        out: &std::path::Path,
        sample_url: &str,
    ) -> Result<()> {
        let out_s = out.to_string_lossy().into_owned();
        let args = [
            "--cookies-from-browser", browser,
            "--cookies", &out_s,
            "--skip-download", "--no-warnings", "--ignore-errors",
            "--playlist-items", "0",
            sample_url,
        ];
        let _ = self.run_inner(&args).await; // куки пишутся даже при ошибке экстракции
        match tokio::fs::metadata(out).await {
            Ok(m) if m.len() > 0 => Ok(()),
            _ => Err(Error::Tool(
                "yt-dlp не экспортировал куки (нет браузера/доступа к Keychain?)".into(),
            )),
        }
    }
}

/// Доступен ли `ffmpeg` в PATH. yt-dlp дёргает его для оконного скачивания
/// (`--download-sections`) и муксинга — без него media-стадия частично сломается,
/// но метаданные/discovery работают. Бинарь можно переопределить env `FFMPEG_BIN`.
pub fn ffmpeg_available() -> bool {
    let bin = std::env::var("FFMPEG_BIN").unwrap_or_else(|_| "ffmpeg".into());
    std::process::Command::new(bin)
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Текущее unix-время. Изолировано здесь, чтобы ядро оставалось без часов.
pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use detox_parser_core::error::Error;

    fn ytdlp_with_pool(with_pool: bool) -> YtDlp {
        let pool = with_pool
            .then(|| Arc::new(ProxyPool::from_list(vec!["http://p1".into()], 30)));
        YtDlp::build(Platform::Youtube, "/tmp", &CookieOpts::default(), pool, None)
    }

    #[test]
    fn bot_gate_is_terminal_without_proxy() {
        let yt = ytdlp_with_pool(false);
        let e = yt.classify("ERROR: Sign in to confirm you're not a bot");
        assert!(matches!(e, Error::Tool(_)), "без пула гейт терминальный (нужны куки)");
        assert!(!e.is_retryable());
    }

    #[test]
    fn bot_gate_is_retryable_with_proxy() {
        let yt = ytdlp_with_pool(true);
        let e = yt.classify("ERROR: Sign in to confirm you're not a bot");
        assert!(matches!(e, Error::Transient(_)), "с пулом гейт → ротация IP");
        assert!(e.is_retryable(), "ретрай попадёт на другой прокси");
    }

    #[test]
    fn rate_limit_and_unavailable_classified_regardless_of_proxy() {
        let yt = ytdlp_with_pool(true);
        assert!(matches!(yt.classify("HTTP Error 429: Too Many Requests"), Error::RateLimited(_)));
        assert!(matches!(yt.classify("Video unavailable"), Error::Unavailable(_)));
    }
}
