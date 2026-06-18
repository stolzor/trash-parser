//! Интеграционные тесты манифеста. Запускаются только при PG_TEST_URL
//! (нужен живой Postgres). Иначе тихо пропускаются.

use detox_parser_core::types::{Candidate, Platform, Provenance, VideoId};
use detox_parser_manifest::{Manifest, Status};

fn candidate(id: &str, source: &str) -> Candidate {
    Candidate {
        video: VideoId::new(Platform::Youtube, id),
        url: format!("https://yt/{id}"),
        provenance: Provenance {
            source: source.into(),
            query: None,
            rank: None,
            fetched_at: 0,
            extractor: "test".into(),
            extractor_version: None,
        },
    }
}

async fn fresh() -> Option<Manifest> {
    let url = std::env::var("PG_TEST_URL").ok()?;
    let m = Manifest::connect(&url).await.expect("connect"); // создаёт схему
    // чистим таблицы между прогонами для изоляции
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .unwrap();
    sqlx::query("TRUNCATE videos, seeds, expanded_channels")
        .execute(&pool)
        .await
        .unwrap();
    Some(m)
}

#[tokio::test]
async fn dedup_and_multihop_and_quota() {
    let Some(m) = fresh().await else {
        eprintln!("skip: PG_TEST_URL не задан");
        return;
    };

    // дедуп upsert (уникальные id для изоляции от других прогонов)
    let c = candidate("pgt_v1", "query:x");
    assert!(m.upsert_candidate(&c).await.unwrap());
    assert!(!m.upsert_candidate(&c).await.unwrap());

    // два видео одного канала → один нерасширённый канал
    assert!(m.upsert_candidate(&candidate("pgt_v2", "query:x")).await.unwrap());
    let v1 = VideoId::new(Platform::Youtube, "pgt_v1");
    let v2 = VideoId::new(Platform::Youtube, "pgt_v2");
    m.mark_meta(&v1, Status::Ok, Some("p1"), Some("UC_pgt"), Some("long"), Some(600.0), None, 1)
        .await
        .unwrap();
    m.mark_meta(&v2, Status::Ok, Some("p2"), Some("UC_pgt"), Some("short"), Some(20.0), None, 1)
        .await
        .unwrap();

    let chans = m.unexpanded_channels(Platform::Youtube, 10).await.unwrap();
    assert!(chans.contains(&"UC_pgt".to_string()));

    m.mark_channel_expanded(Platform::Youtube, "UC_pgt", 1, 2).await.unwrap();
    let chans2 = m.unexpanded_channels(Platform::Youtube, 10).await.unwrap();
    assert!(!chans2.contains(&"UC_pgt".to_string()), "расширённый канал не возвращается");

    // pending_media отдаёт домен и длительность
    let pending = m.pending_media(Platform::Youtube, 2, 10).await.unwrap();
    assert!(pending.iter().all(|(_, _, d, dur)| d.is_some() && dur.is_some()));

    // подсчёт скачанного по доменам
    m.mark_media(&v1, Status::Ok, Some("l.mp4"), None, 3).await.unwrap();
    let counts: std::collections::HashMap<_, _> =
        m.media_ok_by_domain().await.unwrap().into_iter().collect();
    assert_eq!(counts.get("long"), Some(&1));
}
