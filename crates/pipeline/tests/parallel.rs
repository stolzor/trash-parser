//! Тесты распараллеливания на РЕАЛЬНЫХ данных. Запускаются только при
//! PG_TEST_URL (нужен живой Postgres + сеть + yt-dlp в PATH). Иначе пропускаются.
//!
//! Часть A — распределённый захват: реальная discovery наполняет очередь, два
//! конкурентных «воркера» дренят её через claim → проверяем, что claim'ы не
//! пересеклись и покрыли всю выборку (no double-work, ничего не потеряно).
//!
//! Часть B — полный конкурентный harvest: три Pipeline-воркера тянут метаданные
//! реальных видео из общей Postgres-очереди → очередь доведена до терминального
//! состояния, застрявших `claimed` не осталось.

use detox_parser_core::config::AppConfig;
use detox_parser_core::traits::Discoverer;
use detox_parser_core::types::{Platform, Seed, SeedKind};
use detox_parser_manifest::{Manifest, Status};
use detox_parser_pipeline::Pipeline;
use detox_parser_ytdlp::YtDlp;
use std::collections::HashSet;

fn pg_url() -> Option<String> {
    std::env::var("PG_TEST_URL").ok()
}

async fn truncate(pg: &str) {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(pg)
        .await
        .unwrap();
    sqlx::query("TRUNCATE videos, seeds, expanded_channels")
        .execute(&pool)
        .await
        .unwrap();
}

fn seed(value: &str, target: usize) -> Seed {
    Seed {
        platform: Platform::Youtube,
        kind: SeedKind::Query,
        value: value.into(),
        domain_hint: None,
        target: Some(target),
    }
}

/// Дренит очередь метаданных батчами по 2, помечая захваченное `ok`. Возвращает
/// id, которые захватил ИМЕННО этот воркер.
async fn drain(m: &Manifest) -> Vec<String> {
    let mut got = Vec::new();
    loop {
        let batch = m.claim_meta_batch(Platform::Youtube, 0, 2, 1000).await.unwrap();
        if batch.is_empty() {
            break;
        }
        for (v, _url) in batch {
            got.push(v.id.clone());
            m.mark_meta(&v, Status::Ok, Some("p"), None, None, None, None, 1000)
                .await
                .unwrap();
        }
    }
    got
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parallelization_on_real_data() {
    let Some(pg) = pg_url() else {
        eprintln!("skip: PG_TEST_URL не задан (нужен Postgres + сеть)");
        return;
    };

    // ---------- Часть A: распределённый claim на реальной discovery ----------
    let yt = YtDlp::new(Platform::Youtube, std::env::temp_dir());
    let disc = detox_parser_youtube::YoutubeDiscoverer::new(yt, None);
    let cands = match disc.discover(&seed("lofi hip hop", 8)).await {
        Ok(c) if !c.is_empty() => c,
        Ok(_) => {
            eprintln!("skip: discovery вернула пусто (троттлинг/сеть)");
            return;
        }
        Err(e) => {
            eprintln!("skip: discovery упала: {e}");
            return;
        }
    };
    let total: HashSet<String> = cands.iter().map(|c| c.video.id.clone()).collect();

    let m = Manifest::connect(&pg).await.unwrap();
    truncate(&pg).await;
    for c in &cands {
        m.upsert_candidate(c).await.unwrap();
    }

    // два конкурентных воркера дренят одну очередь
    let (a, b) = tokio::join!(drain(&m), drain(&m));

    // НЕ пересеклись (no double-work) и покрыли всю выборку (ничего не потеряно)
    let mut union: HashSet<String> = HashSet::new();
    let mut overlap = 0;
    for id in a.iter().chain(b.iter()) {
        if !union.insert(id.clone()) {
            overlap += 1;
        }
    }
    assert_eq!(overlap, 0, "конкурентные claim'ы пересеклись — double-work!");
    assert_eq!(union, total, "часть видео потеряна или обработана не из очереди");
    assert!(!a.is_empty() && !b.is_empty(), "работа не разделилась между воркерами");
    eprintln!("Часть A: {} видео, воркеры взяли {} и {} (disjoint)", total.len(), a.len(), b.len());

    // ---------- Часть B: полный конкурентный harvest реальных данных ----------
    let tmp = std::env::temp_dir().join(format!("detox_par_{}", std::process::id()));
    let cfg_str = format!(
        r#"
out_root = "{out}"
[manifest]
url = "{pg}"
[media]
download = false
[concurrency]
meta_workers = 2
requests_per_sec = 8
claim_batch = 2
[[seeds]]
platform = "youtube"
kind = "query"
value = "lofi hip hop"
target = 6
"#,
        out = tmp.display(),
        pg = pg,
    );
    let mk = || toml::from_str::<AppConfig>(&cfg_str).unwrap();

    let p0 = Pipeline::new(mk()).await.unwrap(); // создаёт схему
    truncate(&pg).await;
    let found = p0.discover("ptest").await.unwrap();
    if found == 0 {
        eprintln!("skip(B): discovery пустая");
        return;
    }

    // три воркера тянут метаданные из общей очереди одновременно
    let w1 = Pipeline::new(mk()).await.unwrap();
    let w2 = Pipeline::new(mk()).await.unwrap();
    let w3 = Pipeline::new(mk()).await.unwrap();
    let (r1, r2, r3) = tokio::join!(w1.harvest(), w2.harvest(), w3.harvest());
    r1.unwrap();
    r2.unwrap();
    r3.unwrap();

    // очередь доведена до терминала, застрявших claimed нет
    let summary = p0.manifest().summary().await.unwrap();
    let count = |status: &str| -> i64 {
        summary
            .iter()
            .filter(|(stage, s, _)| stage == "meta" && s == status)
            .map(|(_, _, n)| *n)
            .sum()
    };
    let claimed = count("claimed");
    let terminal = count("ok") + count("failed") + count("filtered");
    assert_eq!(claimed, 0, "остались застрявшие claimed-строки");
    assert_eq!(terminal, found as i64, "не все видео доведены до терминала");
    eprintln!("Часть B: {found} видео доведены до терминала тремя воркерами (claimed=0)");
}
