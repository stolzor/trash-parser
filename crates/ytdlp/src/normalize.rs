//! Маппинг сырого `yt-dlp -J` JSON → `NormalizedVideo`.
//!
//! yt-dlp нормализует поля разных платформ к единым именам (view_count,
//! like_count, ...), поэтому один маппер работает и для YouTube, и для TikTok.

use detox_parser_core::types::*;
use serde_json::Value;

fn u64_of(v: &Value, key: &str) -> Option<u64> {
    v.get(key).and_then(|x| x.as_u64())
}

fn f64_of(v: &Value, key: &str) -> Option<f64> {
    v.get(key).and_then(|x| x.as_f64())
}

fn str_of(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(|x| x.as_str()).map(str::to_owned)
}

fn str_array(v: &Value, key: &str) -> Vec<String> {
    v.get(key)
        .and_then(|x| x.as_array())
        .map(|a| a.iter().filter_map(|e| e.as_str().map(str::to_owned)).collect())
        .unwrap_or_default()
}

/// Лучший по `tbr`/наличию полей видео-формат из массива `formats`.
fn pick_format(raw: &Value) -> Option<MediaFormat> {
    // yt-dlp кладёт итоговые поля и на верхний уровень — берём их в первую очередь.
    let width = u64_of(raw, "width").map(|x| x as u32);
    let height = u64_of(raw, "height").map(|x| x as u32);
    let fps = f64_of(raw, "fps");
    let aspect = match (width, height) {
        (Some(w), Some(h)) if h > 0 => Some(w as f64 / h as f64),
        _ => None,
    };
    if width.is_none() && height.is_none() && fps.is_none() {
        return None;
    }
    Some(MediaFormat {
        fps,
        width,
        height,
        aspect_ratio: aspect,
        vcodec: str_of(raw, "vcodec"),
        acodec: str_of(raw, "acodec"),
        tbr: f64_of(raw, "tbr"),
        filesize: u64_of(raw, "filesize").or_else(|| u64_of(raw, "filesize_approx")),
    })
}

fn thumbnails(raw: &Value) -> Vec<Thumbnail> {
    raw.get("thumbnails")
        .and_then(|x| x.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|t| {
                    let url = t.get("url").and_then(|u| u.as_str())?.to_owned();
                    Some(Thumbnail {
                        url,
                        width: u64_of(t, "width").map(|x| x as u32),
                        height: u64_of(t, "height").map(|x| x as u32),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn caption_langs(raw: &Value) -> Vec<String> {
    let mut langs = vec![];
    for key in ["subtitles", "automatic_captions"] {
        if let Some(obj) = raw.get(key).and_then(|x| x.as_object()) {
            langs.extend(obj.keys().cloned());
        }
    }
    langs.sort();
    langs.dedup();
    langs
}

fn categories(raw: &Value) -> Vec<String> {
    let mut cats = str_array(raw, "categories");
    if cats.is_empty() {
        if let Some(c) = str_of(raw, "category") {
            cats.push(c);
        }
    }
    cats
}

/// Построить нормализованную запись из сырого JSON.
pub fn normalize(
    platform: Platform,
    id: &str,
    url: &str,
    raw: &Value,
    provenance: Provenance,
) -> NormalizedVideo {
    let duration_s = f64_of(raw, "duration");
    NormalizedVideo {
        schema_version: SCHEMA_VERSION,
        platform,
        id: id.to_owned(),
        url: url.to_owned(),
        title: str_of(raw, "title"),
        description: str_of(raw, "description"),
        channel: Channel {
            id: str_of(raw, "channel_id").or_else(|| str_of(raw, "uploader_id")),
            name: str_of(raw, "channel").or_else(|| str_of(raw, "uploader")),
            url: str_of(raw, "channel_url").or_else(|| str_of(raw, "uploader_url")),
            follower_count: u64_of(raw, "channel_follower_count"),
        },
        duration_s,
        domain: duration_s.map(Domain::for_duration),
        view_count: u64_of(raw, "view_count"),
        like_count: u64_of(raw, "like_count"),
        comment_count: u64_of(raw, "comment_count"),
        share_count: u64_of(raw, "repost_count"),
        upload_date: str_of(raw, "upload_date"),
        timestamp: raw.get("timestamp").and_then(|x| x.as_i64()),
        categories: categories(raw),
        tags: str_array(raw, "tags"),
        hashtags: str_array(raw, "hashtags"),
        language: str_of(raw, "language"),
        thumbnails: thumbnails(raw),
        format: pick_format(raw),
        captions: caption_langs(raw),
        heatmap: raw.get("heatmap").cloned(),
        chapters: raw.get("chapters").cloned(),
        music: raw.get("track").cloned().or_else(|| raw.get("artist").cloned()),
        provenance,
        raw_path: None,
    }
}
