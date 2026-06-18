//! Интеграционный тест S3-sink. Запускается только при заданном S3_TEST_BUCKET
//! (нужен живой MinIO/S3 + креды в env). Иначе тихо пропускается.

use detox_parser_core::traits::Sink;
use detox_parser_core::types::*;
use detox_parser_store_s3::{S3Settings, S3Store};

fn normalized() -> NormalizedVideo {
    serde_json::from_value(serde_json::json!({
        "schema_version": 1, "platform": "youtube", "id": "vtest", "url": "https://x/vtest",
        "title": "t", "description": null,
        "channel": {"id": "UC1", "name": "c", "url": null, "follower_count": 1},
        "duration_s": 12.0, "domain": "short",
        "view_count": 10, "like_count": 1, "comment_count": 0, "share_count": null,
        "upload_date": "20240101", "timestamp": null,
        "categories": [], "tags": [], "hashtags": [], "language": null,
        "thumbnails": [], "format": null, "captions": [],
        "heatmap": null, "chapters": null, "music": null,
        "provenance": {"source": "test", "query": null, "rank": null,
            "fetched_at": 0, "extractor": "test", "extractor_version": null},
        "raw_path": null
    }))
    .unwrap()
}

#[tokio::test]
async fn s3_roundtrip() {
    let Ok(bucket) = std::env::var("S3_TEST_BUCKET") else {
        eprintln!("skip: S3_TEST_BUCKET не задан (нужен живой MinIO/S3)");
        return;
    };
    let staging = std::env::temp_dir().join("detox_s3test_staging");
    let store = S3Store::new(S3Settings {
        bucket,
        region: "us-east-1".into(),
        endpoint: std::env::var("S3_TEST_ENDPOINT").ok(),
        prefix: "test".into(),
        path_style: true,
        staging_dir: staging.to_string_lossy().into_owned(),
    })
    .await
    .expect("connect");

    let video = VideoId::new(Platform::Youtube, "vtest");
    let extract = RawExtract {
        video: video.clone(),
        raw: serde_json::json!({"id": "vtest", "view_count": 10}),
        normalized: normalized(),
    };

    let raw_ref = store.put_raw(&extract).await.expect("put_raw");
    assert!(raw_ref.path.starts_with("s3://"), "raw ref = {}", raw_ref.path);
    store.put_normalized(&extract.normalized).await.expect("put_normalized");

    // put_media: кладём фейковый файл в staging, ждём загрузку + удаление
    tokio::fs::create_dir_all(store.staging_dir()).await.unwrap();
    let f = store.staging_dir().join("vtest.mp4");
    tokio::fs::write(&f, b"fakevideo").await.unwrap();
    let art = MediaArtifact { video, path: f.to_string_lossy().into_owned(), bytes: 9 };
    store.put_media(&art).await.expect("put_media");
    assert!(!f.exists(), "локальная копия должна быть удалена после загрузки в S3");
}
