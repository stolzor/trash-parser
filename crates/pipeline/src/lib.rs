//! Оркестратор Stage 0/1/1b. Зависит только от трейтов ядра — бэкенды
//! (youtube/tiktok/ytdlp/store/manifest) подставляются при сборке.
//!
//! State machine на видео: Discovered → MetaFetched → Gated → MediaFetched.
//! Идемпотентность и резюмируемость обеспечивает манифест.

use detox_parser_core::config::{AppConfig, DiscoveryConfig, StorageBackend};
use detox_parser_core::error::{Error, Result};
use detox_parser_core::traits::{Discoverer, Extractor, MediaFetcher, Sink};
use detox_parser_core::types::*;
use detox_parser_manifest::{Manifest, Status};
use detox_parser_store::FsStore;
use detox_parser_store_s3::{S3Settings, S3Store};
use detox_parser_ytdlp::{now_unix, CookieOpts, YtDlp};
use futures::stream::{self, StreamExt};
use governor::{Quota, RateLimiter};
use std::collections::HashMap;
use std::num::NonZeroU32;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tracing::{info, warn};

type Limiter = RateLimiter<
    governor::state::NotKeyed,
    governor::state::InMemoryState,
    governor::clock::DefaultClock,
>;

/// Набор бэкендов одной платформы + её rate-limiter.
struct Backend {
    discoverer: Arc<dyn Discoverer>,
    extractor: Arc<dyn Extractor>,
    media: Arc<dyn MediaFetcher>,
    limiter: Arc<Limiter>,
}

pub struct Pipeline {
    cfg: AppConfig,
    manifest: Arc<Manifest>,
    store: Arc<dyn Sink>,
    backends: Vec<(Platform, Backend)>,
    max_retries: u32,
}

impl Pipeline {
    pub async fn new(cfg: AppConfig) -> Result<Self> {
        // out_root нужен под манифест (SQLite локальный) в любом бэкенде
        tokio::fs::create_dir_all(&cfg.out_root).await?;
        let store: Arc<dyn Sink> = match cfg.storage.backend {
            StorageBackend::Fs => Arc::new(FsStore::new(&cfg.out_root).await?),
            StorageBackend::S3 => {
                let s = &cfg.storage.s3;
                if s.bucket.is_empty() {
                    return Err(Error::Config("storage.s3.bucket пуст".into()));
                }
                Arc::new(
                    S3Store::new(S3Settings {
                        bucket: s.bucket.clone(),
                        region: s.region.clone(),
                        endpoint: s.endpoint.clone(),
                        prefix: s.prefix.clone(),
                        path_style: s.path_style,
                        staging_dir: s.staging_dir.clone(),
                    })
                    .await?,
                )
            }
        };
        let manifest = Arc::new(Manifest::open(
            std::path::Path::new(&cfg.out_root).join("manifest.sqlite"),
        )?);

        let rps = NonZeroU32::new(cfg.concurrency.requests_per_sec.max(1)).unwrap();
        let media_dir = store.staging_dir();
        let cookies = CookieOpts {
            from_browser: cfg.ytdlp.cookies_from_browser.clone(),
            file: cfg.ytdlp.cookies_file.clone(),
        };

        // Какие платформы поднимать — по присутствию в seeds (плюс всегда обе для on-demand).
        let mut backends = Vec::new();
        for platform in [Platform::Youtube, Platform::Tiktok] {
            let yt = YtDlp::with_cookies(platform, media_dir.clone(), &cookies);
            let discoverer: Arc<dyn Discoverer> = match platform {
                Platform::Youtube => {
                    Arc::new(detox_parser_youtube::YoutubeDiscoverer::new(yt.clone()))
                }
                Platform::Tiktok => {
                    Arc::new(detox_parser_tiktok::TiktokDiscoverer::new(yt.clone()))
                }
            };
            backends.push((
                platform,
                Backend {
                    discoverer,
                    extractor: Arc::new(yt.clone()),
                    media: Arc::new(yt),
                    limiter: Arc::new(RateLimiter::direct(Quota::per_second(rps))),
                },
            ));
        }

        Ok(Self { cfg, manifest, store, backends, max_retries: 2 })
    }

    fn backend(&self, platform: Platform) -> &Backend {
        &self.backends.iter().find(|(p, _)| *p == platform).expect("backend").1
    }

    // --- Stage 0 -----------------------------------------------------------

    /// Прогнать все seeds, записать кандидатов в манифест (дедуп) и лог провенанса.
    pub async fn discover(&self, run_id: &str) -> Result<usize> {
        let mut log = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.store.discovery_log(run_id))
            .await?;
        let mut new_total = 0usize;

        for seed in &self.cfg.seeds {
            let backend = self.backend(seed.platform);
            backend.limiter.until_ready().await;
            self.manifest.record_seed(run_id, seed, now_unix())?;

            match backend.discoverer.discover(seed).await {
                Ok(candidates) => {
                    info!(platform = %seed.platform, value = %seed.value, found = candidates.len(), "discovered");
                    for c in &candidates {
                        if self.manifest.upsert_candidate(c)? {
                            new_total += 1;
                        }
                        let line = serde_json::to_string(c)?;
                        log.write_all(line.as_bytes()).await?;
                        log.write_all(b"\n").await?;
                    }
                }
                Err(e) => warn!(platform = %seed.platform, value = %seed.value, error = %e, "discover failed"),
            }
        }
        log.flush().await?;
        Ok(new_total)
    }

    // --- Stage 1 -----------------------------------------------------------

    /// Собрать метаданные для всех pending. Резюмируемо, идемпотентно.
    pub async fn harvest(&self) -> Result<()> {
        for (platform, backend) in &self.backends {
            let pending =
                self.manifest.pending_meta(*platform, self.max_retries, 100_000)?;
            if pending.is_empty() {
                continue;
            }
            info!(%platform, count = pending.len(), "harvest meta");

            let workers = self.cfg.concurrency.meta_workers.max(1);
            stream::iter(pending)
                .for_each_concurrent(workers, |(video, url)| {
                    let backend_ex = backend.extractor.clone();
                    let limiter = backend.limiter.clone();
                    let manifest = self.manifest.clone();
                    let store = self.store.clone();
                    let gate = self.cfg.discovery.clone();
                    let max_retries = self.max_retries;
                    async move {
                        limiter.until_ready().await;
                        let res =
                            retry(max_retries, || backend_ex.metadata(&video, &url)).await;
                        match res {
                            Ok(mut extract) => {
                                if let Err(e) = persist_meta(store.as_ref(), &manifest, &mut extract, &gate).await {
                                    warn!(%video, error = %e, "persist meta failed");
                                }
                            }
                            // недоступное видео — терминально, исключаем из выборки
                            Err(Error::Unavailable(msg)) => {
                                let _ = manifest.mark_meta(&video, Status::Filtered, None, None, None, None, Some(&msg), now_unix());
                                let _ = manifest.mark_media(&video, Status::Skipped, None, None, now_unix());
                            }
                            Err(e) => {
                                warn!(%video, error = %e, "meta failed");
                                let _ = manifest.mark_meta(&video, Status::Failed, None, None, None, None, Some(&e.to_string()), now_unix());
                            }
                        }
                    }
                })
                .await;
        }
        Ok(())
    }

    // --- Stage 1b ----------------------------------------------------------

    /// Скачать медиа для прошедших meta+gate.
    pub async fn fetch_media(&self) -> Result<()> {
        if !self.cfg.media.download {
            info!("media download disabled");
            return Ok(());
        }
        let opts = self.cfg.media.to_opts();
        let media_dir = self.store.staging_dir();
        let quota = self.cfg.quota.clone();
        let long_windows = self.cfg.media.long_windows;
        let window_secs = self.cfg.media.long_window_seconds;

        // Счётчики скачанного по доменам — засеваем уже загруженным (резюм-safe),
        // дальше резервируем слоты атомарно, чтобы воркеры не перебрали квоту.
        let mut seed: HashMap<String, usize> = HashMap::new();
        for (d, n) in self.manifest.media_ok_by_domain()? {
            seed.insert(d, n as usize);
        }
        let counters: Arc<HashMap<String, AtomicUsize>> = Arc::new(
            ["short", "long"]
                .iter()
                .map(|d| (d.to_string(), AtomicUsize::new(*seed.get(*d).unwrap_or(&0))))
                .collect(),
        );

        for (platform, backend) in &self.backends {
            let pending =
                self.manifest.pending_media(*platform, self.max_retries, 100_000)?;
            if pending.is_empty() {
                continue;
            }
            info!(%platform, count = pending.len(), "fetch media");

            let workers = self.cfg.concurrency.media_workers.max(1);
            stream::iter(pending)
                .for_each_concurrent(workers, |(video, url, domain, duration)| {
                    let media = backend.media.clone();
                    let limiter = backend.limiter.clone();
                    let manifest = self.manifest.clone();
                    let store = self.store.clone();
                    let opts = opts.clone();
                    let media_dir = media_dir.clone();
                    let counters = counters.clone();
                    let quota = quota.clone();
                    let max_retries = self.max_retries;
                    async move {
                        // --- квота: резервируем слот домена до скачивания ---
                        let reserved = match domain.as_deref() {
                            Some(d) => match (quota.for_domain(d), counters.get(d)) {
                                (Some(limit), Some(counter)) => {
                                    if counter.fetch_add(1, Ordering::SeqCst) >= limit {
                                        counter.fetch_sub(1, Ordering::SeqCst); // откат
                                        let _ = manifest.mark_media(
                                            &video, Status::Skipped, None, Some("quota"), now_unix());
                                        return;
                                    }
                                    counters.get(d) // слот занят, освободим при провале
                                }
                                _ => None,
                            },
                            None => None,
                        };

                        // длинные видео качаем не целиком, а сэмплированными окнами
                        let mut vid_opts = opts.clone();
                        if domain.as_deref() == Some("long") && long_windows > 0 {
                            if let Some(dur) = duration {
                                vid_opts.sections = sample_windows(dur, window_secs, long_windows);
                            }
                        }

                        limiter.until_ready().await;
                        match retry(max_retries, || media.fetch(&video, &url, &vid_opts)).await {
                            Ok(art) => {
                                let _ = store.put_media(&art).await;
                                let _ = manifest.mark_media(&video, Status::Ok, Some(&art.path), None, now_unix());
                            }
                            Err(Error::Unavailable(msg)) => {
                                if let Some(c) = reserved { c.fetch_sub(1, Ordering::SeqCst); }
                                cleanup_partial(&media_dir, &video.id).await;
                                let _ = manifest.mark_media(&video, Status::Skipped, None, Some(&msg), now_unix());
                            }
                            Err(e) => {
                                if let Some(c) = reserved { c.fetch_sub(1, Ordering::SeqCst); }
                                warn!(%video, error = %e, "media failed");
                                cleanup_partial(&media_dir, &video.id).await;
                                let _ = manifest.mark_media(&video, Status::Failed, None, Some(&e.to_string()), now_unix());
                            }
                        }
                    }
                })
                .await;
        }
        Ok(())
    }

    // --- Многохоповое расширение (анти-survivorship-bias) ------------------

    /// Один хоп: из каналов уже собранных видео тянем их полные аплоады.
    /// Возвращает число новых кандидатов. Пока только YouTube (channel_id = UC).
    pub async fn expand(&self, hop: u32) -> Result<usize> {
        let cfg = &self.cfg.discovery;
        let mut new_total = 0usize;
        for (platform, backend) in &self.backends {
            // у TikTok id канала ненадёжен как @handle — расширение пропускаем
            if *platform != Platform::Youtube {
                continue;
            }
            let channels =
                self.manifest.unexpanded_channels(*platform, cfg.max_channels_per_hop)?;
            if channels.is_empty() {
                continue;
            }
            info!(%platform, channels = channels.len(), hop, "expand: pulling full channel uploads");
            for channel in channels {
                backend.limiter.until_ready().await;
                let seed = Seed {
                    platform: *platform,
                    kind: SeedKind::Channel,
                    value: channel.clone(),
                    domain_hint: None,
                    target: Some(cfg.expand_channel_videos),
                };
                match backend.discoverer.discover(&seed).await {
                    Ok(cands) => {
                        for c in &cands {
                            if self.manifest.upsert_candidate(c)? {
                                new_total += 1;
                            }
                        }
                    }
                    Err(e) => warn!(%channel, error = %e, "expand discover failed"),
                }
                self.manifest.mark_channel_expanded(*platform, &channel, hop, now_unix())?;
            }
        }
        Ok(new_total)
    }

    /// Цикл расширения: до `max_hops` хопов, каждый раз добирая метаданные новых.
    /// Останавливается раньше, если хоп не дал новых кандидатов.
    pub async fn expand_loop(&self) -> Result<()> {
        for hop in 1..=self.cfg.discovery.max_hops {
            let added = self.expand(hop).await?;
            info!(hop, added, "expansion hop done");
            if added == 0 {
                break;
            }
            self.harvest().await?;
        }
        Ok(())
    }

    /// Полный прогон: discover → harvest → expand* → media.
    pub async fn run(&self, run_id: &str) -> Result<()> {
        let new = self.discover(run_id).await?;
        info!(new_candidates = new, "discovery done");
        self.harvest().await?;
        self.expand_loop().await?;
        self.fetch_media().await?;
        Ok(())
    }

    /// Разовый парс одного URL (debug / on-demand), без discovery.
    pub async fn single(&self, platform: Platform, url: &str) -> Result<NormalizedVideo> {
        let id = url
            .rsplit(['/', '=']) // грубое выделение id; нормализуется экстрактором
            .next()
            .unwrap_or(url)
            .to_string();
        let video = VideoId::new(platform, id);
        let backend = self.backend(platform);
        let mut extract = backend.extractor.metadata(&video, url).await?;
        persist_meta(self.store.as_ref(), &self.manifest, &mut extract, &self.cfg.discovery).await?;
        Ok(extract.normalized)
    }

    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }
}

/// Лёгкий gate перед медиа. Авторитетный фильтр — Silver в ml.
fn passes_gate(v: &NormalizedVideo, cfg: &DiscoveryConfig) -> bool {
    match v.duration_s {
        Some(d) => d >= cfg.min_duration_s,
        None => false,
    }
}

/// Записать raw + normalized и проставить статусы.
async fn persist_meta(
    store: &dyn Sink,
    manifest: &Manifest,
    extract: &mut RawExtract,
    gate: &DiscoveryConfig,
) -> Result<()> {
    let raw_ref = store.put_raw(extract).await?;
    extract.normalized.raw_path = Some(raw_ref.path.clone());
    store.put_normalized(&extract.normalized).await?;

    let now = now_unix();
    let channel_id = extract.normalized.channel.id.clone();
    let domain = extract.normalized.domain.map(|d| d.as_str());
    manifest.mark_meta(
        &extract.video,
        Status::Ok,
        Some(&raw_ref.path),
        channel_id.as_deref(),
        domain,
        extract.normalized.duration_s,
        None,
        now,
    )?;

    // gate решает, качать ли медиа
    if gate.gate_before_media && !passes_gate(&extract.normalized, gate) {
        manifest.mark_media(&extract.video, Status::Skipped, None, Some("gate"), now)?;
    }
    Ok(())
}

/// `n` равномерно распределённых окон длины `win` по ролику длительности `dur`.
/// Пустой результат = качать целиком (ролик короче окна / окна не нужны).
/// Для `n>1`: первое окно с начала, последнее — к концу, остальные между.
fn sample_windows(dur: f64, win: f64, n: usize) -> Vec<(f64, f64)> {
    if n == 0 || dur <= win {
        return Vec::new();
    }
    let last_start = dur - win;
    if n == 1 {
        let s = (last_start / 2.0).round();
        return vec![(s, s + win)];
    }
    (0..n)
        .map(|i| {
            let s = (last_start * i as f64 / (n - 1) as f64).round();
            (s, s + win)
        })
        .collect()
}

/// Удалить недокачанные фрагменты (`<id>.*.part`, `.ytdl`) после провала.
async fn cleanup_partial(media_dir: &std::path::Path, id: &str) {
    let Ok(mut rd) = tokio::fs::read_dir(media_dir).await else { return };
    while let Ok(Some(entry)) = rd.next_entry().await {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with(id) && (name.ends_with(".part") || name.ends_with(".ytdl")) {
            let _ = tokio::fs::remove_file(entry.path()).await;
        }
    }
}

/// Ретрай с экспоненциальной задержкой только для временных ошибок.
async fn retry<T, Fut, F>(max: u32, mut f: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    let mut attempt = 0u32;
    loop {
        match f().await {
            Ok(v) => return Ok(v),
            Err(e) if e.is_retryable() && attempt < max => {
                let secs = (1u64 << attempt).min(30);
                tokio::time::sleep(Duration::from_secs(secs)).await;
                attempt += 1;
            }
            Err(e) => return Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::sample_windows;

    #[test]
    fn whole_download_when_not_windowable() {
        assert!(sample_windows(30.0, 30.0, 4).is_empty(), "ролик ≤ окна → целиком");
        assert!(sample_windows(600.0, 30.0, 0).is_empty(), "n=0 → целиком");
    }

    #[test]
    fn long_video_samples_evenly() {
        let w = sample_windows(600.0, 30.0, 3);
        assert_eq!(w, vec![(0.0, 30.0), (285.0, 315.0), (570.0, 600.0)]);
        // все окна в пределах ролика
        assert!(w.iter().all(|(s, e)| *s >= 0.0 && *e <= 600.0));
    }

    #[test]
    fn single_window_is_centered() {
        assert_eq!(sample_windows(100.0, 30.0, 1), vec![(35.0, 65.0)]);
    }
}
