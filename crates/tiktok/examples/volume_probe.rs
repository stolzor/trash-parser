//! Объёмный harness: гоняет оба пути сбора TikTok по N видео и замеряет, где (и
//! начинает ли) деградировать success-rate — это открытый вопрос ADR-0004
//! («нужны ли вообще прогретые сессии»).
//!
//! Для каждого видео делает две независимые пробы:
//!   - нативный SSR (`TikTokWeb::fetch_page`) — отдала ли страница встроенный
//!     JSON или бот-стену/регион/429 (быстрый путь, который сессии бы лечили);
//!   - yt-dlp `-J` — успех + покрытие 9 ML-полей (боевой путь сбора).
//!
//! Печатает success-rate ПО ОКНАМ (чтобы поймать момент, когда IP начинает
//! стенить), разбивку по классам отказов и покрытие полей; пишет per-request CSV.
//!
//! Запуск:
//!   cargo run -p detox-parser-tiktok --example volume_probe -- @tiktok 100
//!   cargo run -p detox-parser-tiktok --example volume_probe -- "#running" 200
//!
//! Переменные окружения:
//!   TIKTOK_PROXY_FILE   список sticky-residential прокси (по одному на строку)
//!   TIKTOK_COOKIE_FILE  Netscape cookies.txt (идёт и в нативный путь, и в yt-dlp)
//!   TIKTOK_COOKIE       готовая строка заголовка Cookie (только нативный путь)
//!   TIKTOK_PROBE_DELAY_MS  пауза между видео, мс (по умолч. 500 — чтобы не
//!                          схватить 429 от собственного темпа)
//!   TIKTOK_PROBE_WINDOW    размер окна для success-rate (по умолч. 25)
//!   TIKTOK_PROBE_NATIVE=0  отключить нативную пробу
//!   TIKTOK_PROBE_OUT       путь CSV (по умолч. tiktok_probe.csv)
//!   YTDLP_BIN              путь к бинарю yt-dlp
//!
//! Авто-режим поиска безопасного темпа (`TIKTOK_PROBE_AUTO=1`): рампит
//! конкурренси (одиночный yt-dlp упирается в ~12 req/min, параллелизм поднимает
//! реальный темп), после каждого раунда меряет wall-rate (без `unavailable` —
//! это контент, не стена) и останавливается на первой стене, печатая последний
//! безопасный темп req/min на узел.
//!   TIKTOK_PROBE_AUTO=1   включить авто-режим (нативная проба отключается)
//!   TIKTOK_PROBE_ROUND    видео на раунд (по умолч. 30)
//!   TIKTOK_PROBE_MAXCONC  до какой конкурренси рампить (по умолч. 6)
//!   TIKTOK_PROBE_WALLPCT  порог wall-rate в %, выше которого темп «небезопасен» (10)
//!   TIKTOK_PROBE_COOLDOWN_S  пауза между раундами, чтобы узел остыл (20)
//!
//!   cargo run -p detox-parser-tiktok --example volume_probe -- @tiktok 200
//!     # с TIKTOK_PROBE_AUTO=1 → ищет потолок темпа вместо одиночного прогона

use std::collections::BTreeMap;
use std::io::Write;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use detox_parser_core::{Error, Extractor, NormalizedVideo, Platform, ProxyPool, VideoId};
use detox_parser_tiktok::{cookie_header_from_file, web::TikTokWeb};
use detox_parser_ytdlp::{CookieOpts, YtDlp};

const FIELD_NAMES: [&str; 9] = [
    "view_count", "like_count", "comment_count", "share_count", "duration",
    "upload_date", "timestamp", "channel", "title",
];

fn field_bits(n: &NormalizedVideo) -> [bool; 9] {
    [
        n.view_count.is_some(),
        n.like_count.is_some(),
        n.comment_count.is_some(),
        n.share_count.is_some(),
        n.duration_s.is_some(),
        n.upload_date.is_some(),
        n.timestamp.is_some(),
        n.channel.name.is_some(),
        n.title.is_some(),
    ]
}

/// Класс исхода для статистики (нативный путь и yt-dlp мапятся одинаково).
fn err_label(e: &Error) -> &'static str {
    match e {
        Error::RateLimited(_) => "rate_limited",
        Error::RegionBlocked(_) => "region_blocked",
        Error::Transient(_) => "transient",
        Error::Parse(_) => "botwall/parse",
        Error::Unavailable(_) => "unavailable",
        Error::Tool(_) => "tool",
        _ => "other",
    }
}

fn unix() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let seed = args.get(1).cloned().unwrap_or_else(|| "@tiktok".into());
    let n: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(100);
    let delay_ms: u64 = env_num("TIKTOK_PROBE_DELAY_MS", 500);
    let window: usize = env_num("TIKTOK_PROBE_WINDOW", 25) as usize;
    let out_path = std::env::var("TIKTOK_PROBE_OUT").unwrap_or_else(|_| "tiktok_probe.csv".into());
    let native_on = std::env::var("TIKTOK_PROBE_NATIVE").map(|v| v != "0").unwrap_or(true);

    // --- прокси ---
    let proxy = std::env::var("TIKTOK_PROXY_FILE").ok().and_then(|p| {
        match ProxyPool::from_file(&p, 30) {
            Ok(pool) => {
                eprintln!("proxy: {} узлов из {p}", pool.len());
                Some(Arc::new(pool))
            }
            Err(e) => {
                eprintln!("proxy: не загружен ({e}) — работаем без прокси");
                None
            }
        }
    });

    // --- куки: нативный путь хочет строку-заголовок, yt-dlp — файл ---
    let cookie_file = std::env::var("TIKTOK_COOKIE_FILE").ok();
    let mut native_cookie = std::env::var("TIKTOK_COOKIE")
        .ok()
        .or_else(|| cookie_file.as_deref().and_then(cookie_header_from_file));

    let cookies = CookieOpts { from_browser: None, file: cookie_file.clone() };
    let media_dir = std::env::temp_dir().join("tiktok_probe_media");
    let mut web = TikTokWeb::new(proxy.clone(), native_cookie.clone());
    // проба меряет только метаданные — оба пула = основной прокси
    let yt = YtDlp::build(
        Platform::Tiktok,
        media_dir.clone(),
        &cookies,
        proxy.clone(),
        proxy.clone(),
    );

    // Тир A: если cookie не задана — добыть анонимные куки доверия без браузера.
    if native_cookie.is_none() {
        match web.bootstrap_cookies().await {
            Ok(Some(c)) => {
                eprintln!("cookie: bootstrap анонимных кук ок ({} симв.)", c.len());
                web = TikTokWeb::new(proxy.clone(), Some(c.clone()));
                native_cookie = Some(c);
            }
            Ok(None) => eprintln!("cookie: bootstrap не вернул кук"),
            Err(e) => eprintln!("cookie: bootstrap не удался ({e})"),
        }
    }
    eprintln!(
        "cookie: native={}, ytdlp_file={}",
        if native_cookie.is_some() { "есть" } else { "нет" },
        cookie_file.as_deref().unwrap_or("нет")
    );

    let (page_url, spec) = if seed.starts_with('@') {
        let u = format!("https://www.tiktok.com/{seed}");
        (u.clone(), u)
    } else {
        let tag = seed.trim_start_matches('#');
        let u = format!("https://www.tiktok.com/tag/{tag}");
        (u.clone(), u)
    };

    // --- сравнение discovery ---
    println!("== Discovery (seed = {seed}, target = {n}) ==");
    match web.discover_url(&page_url, n).await {
        Ok(items) => println!("  native SSR : {} items", items.len()),
        Err(e) => println!("  native SSR : FAIL [{}] {e}", err_label(&e)),
    }
    let entries = match yt.flat_playlist(&spec, n).await {
        Ok(es) => {
            println!("  yt-dlp     : {} items", es.len());
            es
        }
        Err(e) => {
            println!("  yt-dlp     : FAIL [{}] {e}", err_label(&e));
            eprintln!("нет списка видео для harvest — выходим");
            return;
        }
    };
    if entries.is_empty() {
        eprintln!("список видео пуст — выходим");
        return;
    }

    // --- авто-режим: поиск безопасного темпа (вместо одиночного прогона) ---
    if std::env::var("TIKTOK_PROBE_AUTO").map(|v| v != "0").unwrap_or(false) {
        let videos: Vec<(String, String)> = entries
            .iter()
            .map(|e| {
                let url = if e.url.starts_with("http") {
                    e.url.clone()
                } else {
                    format!("https://www.tiktok.com/@_/video/{}", e.id)
                };
                (e.id.clone(), url)
            })
            .collect();
        run_auto(&yt, &videos).await;
        return;
    }

    // --- harvest по объёму ---
    let mut csv = std::fs::File::create(&out_path).expect("создать CSV");
    writeln!(csv, "idx,unix,id,native,ytdlp,fields_present").ok();

    let mut yt_ok_flags: Vec<bool> = Vec::new();
    let mut native_labels: BTreeMap<&str, usize> = BTreeMap::new();
    let mut yt_labels: BTreeMap<&str, usize> = BTreeMap::new();
    let mut field_counts = [0usize; 9];
    let mut yt_ok = 0usize;
    let mut native_ok = 0usize;

    let started = Instant::now();
    println!("\n== Harvest ({} видео, пауза {delay_ms}мс) ==", entries.len());
    for (idx, e) in entries.iter().enumerate() {
        let url = if e.url.starts_with("http") {
            e.url.clone()
        } else {
            format!("https://www.tiktok.com/@_/video/{}", e.id)
        };

        // нативная проба (страницу не парсим — меряем только проходимость)
        let native = if native_on {
            match web.fetch_page(&url).await {
                Ok(_) => {
                    native_ok += 1;
                    "ok"
                }
                Err(err) => err_label(&err),
            }
        } else {
            "skip"
        };
        *native_labels.entry(native).or_default() += 1;

        // боевая проба: yt-dlp -J + покрытие полей
        let vid = VideoId::new(Platform::Tiktok, e.id.clone());
        let (yt_res, present) = match yt.metadata(&vid, &url).await {
            Ok(raw) => {
                yt_ok += 1;
                yt_ok_flags.push(true);
                let bits = field_bits(&raw.normalized);
                for (i, b) in bits.iter().enumerate() {
                    if *b {
                        field_counts[i] += 1;
                    }
                }
                ("ok", bits.iter().filter(|b| **b).count())
            }
            Err(err) => {
                yt_ok_flags.push(false);
                (err_label(&err), 0)
            }
        };
        *yt_labels.entry(yt_res).or_default() += 1;

        writeln!(csv, "{idx},{},{},{native},{yt_res},{present}", unix(), e.id).ok();

        if (idx + 1) % window == 0 || idx + 1 == entries.len() {
            let w = &yt_ok_flags[idx + 1 - (idx + 1).min(window)..];
            let ok = w.iter().filter(|b| **b).count();
            eprint!(
                "\r  {}/{}  последнее окно yt-dlp: {ok}/{}  ",
                idx + 1,
                entries.len(),
                w.len()
            );
        }
        if delay_ms > 0 && idx + 1 < entries.len() {
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        }
    }
    eprintln!();
    let elapsed = started.elapsed();
    let total = yt_ok_flags.len();

    // --- success-rate по окнам ---
    println!("\n== Success-rate по окнам ({window} запросов) ==");
    for (wi, chunk) in yt_ok_flags.chunks(window).enumerate() {
        let ok = chunk.iter().filter(|b| **b).count();
        let pct = 100.0 * ok as f64 / chunk.len() as f64;
        let bar = "#".repeat((pct / 5.0) as usize);
        println!(
            "  окно {:>2} [{:>4}..]: {:>3}/{:<3} {:>3.0}%  {bar}",
            wi + 1,
            wi * window,
            ok,
            chunk.len(),
            pct
        );
    }

    // --- разбивка по классам отказов ---
    println!("\n== yt-dlp исходы ==");
    for (k, v) in &yt_labels {
        println!("  {k:16} {v}");
    }
    if native_on {
        println!("== native SSR исходы ==");
        for (k, v) in &native_labels {
            println!("  {k:16} {v}");
        }
    }

    // --- покрытие полей ---
    println!("\n== Покрытие ML-полей (среди {yt_ok} успешных yt-dlp) ==");
    for (i, name) in FIELD_NAMES.iter().enumerate() {
        let c = field_counts[i];
        let pct = if yt_ok > 0 { 100.0 * c as f64 / yt_ok as f64 } else { 0.0 };
        println!("  {name:16} {c:>4}/{yt_ok:<4} {pct:>3.0}%");
    }

    // --- итог ---
    let yt_pct = if total > 0 { 100.0 * yt_ok as f64 / total as f64 } else { 0.0 };
    let nat_pct = if total > 0 { 100.0 * native_ok as f64 / total as f64 } else { 0.0 };
    println!("\n== Итог ==");
    println!("  видео обработано : {total}");
    println!("  yt-dlp success   : {yt_ok}/{total}  ({yt_pct:.0}%)");
    if native_on {
        println!("  native success   : {native_ok}/{total}  ({nat_pct:.0}%)");
    }
    println!("  время            : {:.1}s  ({:.2} видео/с)", elapsed.as_secs_f64(), total as f64 / elapsed.as_secs_f64());
    println!("  CSV              : {out_path}");
    println!(
        "\nПодсказка: ровный success-rate по окнам → сессии НЕ нужны (ADR-0004);\n\
         падение rate к поздним окнам → IP стенится во времени → объёмное оправдание сессий."
    );
}

fn env_num(key: &str, default: u64) -> u64 {
    std::env::var(key).ok().and_then(|s| s.parse().ok()).unwrap_or(default)
}

/// Одна боевая проба: категория исхода (0 = ok, 1 = стена, 2 = контент/unavailable).
async fn harvest_one(yt: YtDlp, id: String, url: String) -> u8 {
    let vid = VideoId::new(Platform::Tiktok, id);
    match yt.metadata(&vid, &url).await {
        Ok(_) => 0,
        Err(Error::Unavailable(_)) => 2,
        Err(_) => 1,
    }
}

/// Прогнать срез видео с заданной конкурренси. Возвращает (ok, walls, content,
/// wall-clock). Конкурренси — это и есть рычаг реального темпа: одиночный yt-dlp
/// упирается в латентность субпроцесса, параллелизм поднимает req/min.
async fn run_round(
    yt: &YtDlp,
    slice: &[(String, String)],
    conc: usize,
) -> (usize, usize, usize, Duration) {
    let t0 = Instant::now();
    let mut set: tokio::task::JoinSet<u8> = tokio::task::JoinSet::new();
    let mut iter = slice.iter().cloned();
    for _ in 0..conc.max(1) {
        if let Some((id, url)) = iter.next() {
            set.spawn(harvest_one(yt.clone(), id, url));
        }
    }
    let (mut ok, mut walls, mut content) = (0usize, 0usize, 0usize);
    while let Some(res) = set.join_next().await {
        match res.unwrap_or(1) {
            0 => ok += 1,
            2 => content += 1,
            _ => walls += 1,
        }
        if let Some((id, url)) = iter.next() {
            set.spawn(harvest_one(yt.clone(), id, url));
        }
    }
    (ok, walls, content, t0.elapsed())
}

/// Авто-поиск безопасного темпа: рампит конкурренси, пока wall-rate не превысит
/// порог. Останавливается на ПЕРВОЙ стене (чтобы не банить узел всерьёз и не
/// загрязнять замер) и печатает последний безопасный req/min.
async fn run_auto(yt: &YtDlp, videos: &[(String, String)]) {
    let r = (env_num("TIKTOK_PROBE_ROUND", 30) as usize).max(1);
    let maxconc = (env_num("TIKTOK_PROBE_MAXCONC", 6) as usize).max(1);
    let wallpct = env_num("TIKTOK_PROBE_WALLPCT", 10) as f64;
    let cooldown = env_num("TIKTOK_PROBE_COOLDOWN_S", 20);

    println!("\n== Авто-поиск безопасного темпа ==");
    println!("  round={r} видео, maxconc={maxconc}, wall-порог={wallpct:.0}%, cooldown={cooldown}s");
    if videos.len() < r {
        eprintln!("  ⚠ пул {} < round {r}: видео будут повторяться между раундами", videos.len());
    }
    println!(
        "\n  {:>3} {:>4} {:>8}  {:>4} {:>4} {:>4} {:>7}  verdict",
        "rnd", "conc", "req/min", "ok", "wall", "cont", "wall%"
    );

    let mut cursor = 0usize;
    let mut last_safe: Option<(f64, usize)> = None;
    let mut hit_wall = false;
    for round in 0..maxconc {
        let conc = round + 1;
        let slice: Vec<(String, String)> =
            (0..r).map(|i| videos[(cursor + i) % videos.len()].clone()).collect();
        cursor = (cursor + r) % videos.len();

        let (ok, walls, content, elapsed) = run_round(yt, &slice, conc).await;
        let rpm = r as f64 / elapsed.as_secs_f64().max(0.001) * 60.0;
        let denom = (ok + walls).max(1);
        let wall_rate = 100.0 * walls as f64 / denom as f64;
        let safe = wall_rate <= wallpct;
        println!(
            "  {round:>3} {conc:>4} {rpm:>8.1}  {ok:>4} {walls:>4} {content:>4} {wall_rate:>6.1}%  {}",
            if safe { "OK" } else { "СТЕНА" }
        );

        if !safe {
            hit_wall = true;
            break;
        }
        last_safe = Some((rpm, conc));
        if conc < maxconc && cooldown > 0 {
            eprintln!("  cooldown {cooldown}s, чтобы узел остыл перед следующим темпом…");
            tokio::time::sleep(Duration::from_secs(cooldown)).await;
        }
    }

    println!();
    match last_safe {
        Some((rpm, conc)) => {
            println!(
                "Безопасный темп ≈ {rpm:.0} req/min на узел (конкурренси {conc}, wall-rate ≤ {wallpct:.0}%)."
            );
            println!("Пул из K узлов → суммарно ≈ {rpm:.0}·K req/min.");
            if hit_wall {
                println!("Выше этого темпа узел начал стенить — это его потолок.");
            } else {
                println!(
                    "До maxconc={maxconc} стены не встретили — потолок ещё выше; \
                     подними TIKTOK_PROBE_MAXCONC."
                );
            }
        }
        None => println!(
            "Даже минимальный темп (conc 1) дал wall-rate выше порога — узел уже подсажен, \
             нужен другой IP или валидные куки."
        ),
    }
}
