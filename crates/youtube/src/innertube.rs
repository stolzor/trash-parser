//! Нативный клиент YouTube InnerTube (приватный API, на котором работает сам
//! сайт). Без скачивания/cipher — только discovery: поиск и аплоады канала.
//!
//! Устойчивость к изменениям раскладки: парсинг идёт **рекурсивным обходом**
//! ответа (собираем все `videoId` и continuation-токены), а не по жёстким путям.

use detox_parser_core::error::{Error, Result};
use detox_parser_core::ProxyPool;
use detox_parser_http::ProxyHttp;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::sync::Arc;

/// Публичный API-ключ WEB-клиента (захардкожен в самом youtube.com).
const WEB_KEY: &str = "AIzaSyAO_FJ2SlqU8Q4STEHLGCilw_Y9_11qcW8";
/// Версия WEB-клиента. InnerTube обычно терпим к версии; при поломках — бампнуть.
const CLIENT_VERSION: &str = "2.20240814.01.00";
/// Протобуф-параметр, выбирающий вкладку «Видео» канала (стабильная константа).
const CHANNEL_VIDEOS_PARAMS: &str = "EgZ2aWRlb3PyBgQKAjoA";

const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                  (KHTML, like Gecko) Chrome/127.0.0.0 Safari/537.36";

/// Найденное видео (discovery, без полной метадаты).
#[derive(Debug, Clone)]
pub struct VideoHit {
    pub id: String,
}

pub struct InnerTube {
    http: ProxyHttp,
}

impl InnerTube {
    pub fn new(proxy: Option<Arc<ProxyPool>>) -> Self {
        Self { http: ProxyHttp::new(proxy, UA) }
    }

    fn context() -> Value {
        json!({
            "client": {
                "clientName": "WEB",
                "clientVersion": CLIENT_VERSION,
                "hl": "en",
                "gl": "US"
            }
        })
    }

    /// POST на youtubei/v1/<endpoint> с классификацией ошибок.
    async fn post(&self, endpoint: &str, body: &Value) -> Result<Value> {
        let url = format!(
            "https://www.youtube.com/youtubei/v1/{endpoint}?key={WEB_KEY}&prettyPrint=false"
        );
        let (client, proxy) = self.http.pick();
        let resp = match client.post(&url).json(body).send().await {
            Ok(r) => r,
            Err(e) => {
                self.http.report(&proxy, false);
                return Err(Error::Transient(format!("innertube {endpoint}: {e}")));
            }
        };

        let status = resp.status();
        if status.as_u16() == 429 {
            self.http.report(&proxy, false);
            return Err(Error::RateLimited(format!("innertube {endpoint} 429")));
        }
        if !status.is_success() {
            self.http.report(&proxy, false);
            return Err(Error::Transient(format!("innertube {endpoint} HTTP {status}")));
        }
        self.http.report(&proxy, true);
        resp.json::<Value>()
            .await
            .map_err(|e| Error::Parse(format!("innertube {endpoint} JSON: {e}")))
    }

    /// Поиск по запросу с пагинацией по continuation.
    pub async fn search(&self, query: &str, limit: usize) -> Result<Vec<VideoHit>> {
        let mut body = json!({ "context": Self::context(), "query": query });
        self.paginate("search", &mut body, limit).await
    }

    /// Аплоады канала (вкладка «Видео»). Принимает UC-id, @handle или имя.
    pub async fn channel_videos(&self, handle_or_id: &str, limit: usize) -> Result<Vec<VideoHit>> {
        let browse_id = self.resolve_channel(handle_or_id).await?;
        let mut body = json!({
            "context": Self::context(),
            "browseId": browse_id,
            "params": CHANNEL_VIDEOS_PARAMS
        });
        self.paginate("browse", &mut body, limit).await
    }

    /// Общий цикл: запрос → собрать id → если мало, взять continuation → повтор.
    async fn paginate(&self, endpoint: &str, body: &mut Value, limit: usize) -> Result<Vec<VideoHit>> {
        let mut hits = Vec::new();
        let mut seen = HashSet::new();
        let ctx = Self::context();
        // защита от бесконечного цикла, если continuation не двигает выдачу
        for _ in 0..50 {
            let resp = self.post(endpoint, body).await?;
            collect_video_ids(&resp, &mut hits, &mut seen);
            if hits.len() >= limit {
                break;
            }
            match find_continuation(&resp) {
                Some(token) => {
                    *body = json!({ "context": ctx, "continuation": token });
                }
                None => break,
            }
        }
        hits.truncate(limit);
        Ok(hits)
    }

    /// @handle / имя / URL → browseId (UC...). UC-id возвращается как есть.
    async fn resolve_channel(&self, value: &str) -> Result<String> {
        if value.starts_with("UC") && value.len() == 24 {
            return Ok(value.to_string());
        }
        let url = if value.starts_with("http") {
            value.to_string()
        } else if let Some(handle) = value.strip_prefix('@') {
            format!("https://www.youtube.com/@{handle}")
        } else {
            format!("https://www.youtube.com/@{value}")
        };
        let body = json!({ "context": Self::context(), "url": url });
        let resp = self.post("navigation/resolve_url", &body).await?;
        find_browse_id(&resp)
            .ok_or_else(|| Error::Parse(format!("не удалось разрешить канал '{value}'")))
    }
}

// --- Рекурсивные парсеры (устойчивы к изменениям раскладки) -----------------

/// Собрать все id видео из ответа. Поддерживает обе раскладки YouTube:
/// старую (`videoRenderer`/`gridVideoRenderer`: `videoId` рядом с `title`) и
/// новую (`lockupViewModel`: `contentId` + `contentType=...VIDEO`).
fn collect_video_ids(v: &Value, out: &mut Vec<VideoHit>, seen: &mut HashSet<String>) {
    match v {
        Value::Object(map) => {
            // старая раскладка: карточка видео = videoId рядом с заголовком
            if let (Some(Value::String(id)), true) =
                (map.get("videoId"), map.contains_key("title") || map.contains_key("headline"))
            {
                if seen.insert(id.clone()) {
                    out.push(VideoHit { id: id.clone() });
                }
            }
            // новая раскладка: lockupViewModel с contentType видео
            if let (Some(Value::String(cid)), Some(Value::String(ct))) =
                (map.get("contentId"), map.get("contentType"))
            {
                if ct.contains("VIDEO") && seen.insert(cid.clone()) {
                    out.push(VideoHit { id: cid.clone() });
                }
            }
            for val in map.values() {
                collect_video_ids(val, out, seen);
            }
        }
        Value::Array(arr) => {
            for val in arr {
                collect_video_ids(val, out, seen);
            }
        }
        _ => {}
    }
}

/// Первый continuation-токен в дереве.
fn find_continuation(v: &Value) -> Option<String> {
    match v {
        Value::Object(map) => {
            if let Some(tok) = map
                .get("continuationCommand")
                .and_then(|c| c.get("token"))
                .and_then(|t| t.as_str())
            {
                return Some(tok.to_string());
            }
            map.values().find_map(find_continuation)
        }
        Value::Array(arr) => arr.iter().find_map(find_continuation),
        _ => None,
    }
}

/// Первый browseId вида UC… в дереве (для resolve_url).
fn find_browse_id(v: &Value) -> Option<String> {
    match v {
        Value::Object(map) => {
            if let Some(id) = map
                .get("browseEndpoint")
                .and_then(|b| b.get("browseId"))
                .and_then(|x| x.as_str())
            {
                if id.starts_with("UC") {
                    return Some(id.to_string());
                }
            }
            map.values().find_map(find_browse_id)
        }
        Value::Array(arr) => arr.iter().find_map(find_browse_id),
        _ => None,
    }
}
