//! Async-обёртка над `yt-dlp` (subprocess). Реализует `Extractor` и
//! `MediaFetcher` ядра. Брутальную reverse-engineering часть (cipher,
//! n-throttling, протухающие URL) держит yt-dlp, мы лишь оркестрируем.

mod normalize;

use detox_parser_core::error::{Error, Result};
use detox_parser_core::traits::{Extractor, MediaFetcher};
use detox_parser_core::types::*;
use std::path::PathBuf;
use std::process::Stdio;
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
}

impl YtDlp {
    pub fn new(platform: Platform, media_dir: impl Into<PathBuf>) -> Self {
        Self::with_cookies(platform, media_dir, &CookieOpts::default())
    }

    pub fn with_cookies(
        platform: Platform,
        media_dir: impl Into<PathBuf>,
        cookies: &CookieOpts,
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
        }
    }

    /// Классификация stderr yt-dlp в наш тип ошибок (ретраить или нет).
    fn classify(stderr: &str) -> Error {
        let s = stderr.to_lowercase();
        if s.contains("http error 429") || s.contains("too many requests") {
            Error::RateLimited(stderr.trim().to_owned())
        } else if s.contains("confirm you") || s.contains("sign in") {
            // бот-гейтинг — лечится только куками, ретрай без них бесполезен
            Error::Tool(format!(
                "{} [подсказка: задай [ytdlp] cookies_from_browser или cookies_file]",
                stderr.trim()
            ))
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

    async fn run(&self, args: &[&str]) -> Result<std::process::Output> {
        debug!(?args, "yt-dlp");
        let out = Command::new(&self.bin)
            .args(&self.cookie_args)
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await
            .map_err(|e| {
                Error::Tool(format!("не удалось запустить '{}': {e} (установлен ли yt-dlp?)", self.bin))
            })?;
        if !out.status.success() {
            return Err(Self::classify(&String::from_utf8_lossy(&out.stderr)));
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
            .run(&["-J", "--no-warnings", "--no-playlist", url])
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
        let out = self.run(&arg_refs).await?;

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
            .run(&[
                "--flat-playlist",
                "--no-warnings",
                "-J",
                "--playlist-end",
                &lim,
                target,
            ])
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

/// Текущее unix-время. Изолировано здесь, чтобы ядро оставалось без часов.
pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
