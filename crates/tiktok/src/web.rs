//! Нативный best-effort discovery TikTok без подписи запросов.
//!
//! TikTok встраивает состояние страницы в HTML (`__UNIVERSAL_DATA_FOR_REHYDRATION__`,
//! раньше `SIGI_STATE`). Делаем GET страницы тега/пользователя и достаём оттуда
//! список видео. Это даёт обычно только первую «порцию» (дальше нужен подписанный
//! XHR — туда не лезем); пробелы закрывает yt-dlp fallback в lib.rs.

use detox_parser_core::error::{Error, Result};
use detox_parser_core::ProxyPool;
use detox_parser_http::ProxyHttp;
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                  (KHTML, like Gecko) Chrome/127.0.0.0 Safari/537.36";

/// Видео, найденное на странице (id + автор для канонического URL).
#[derive(Debug, Clone)]
pub struct TikTokItem {
    pub id: String,
    pub username: Option<String>,
}

pub struct TikTokWeb {
    http: ProxyHttp,
    /// Готовая строка заголовка Cookie (msToken/ttwid/…), если задана.
    cookie: Option<String>,
}

impl TikTokWeb {
    pub fn new(proxy: Option<Arc<ProxyPool>>, cookie: Option<String>) -> Self {
        Self { http: ProxyHttp::new(proxy, UA), cookie }
    }

    /// Скачать HTML страницы и распарсить встроенное состояние TikTok в JSON.
    ///
    /// Общий нижний слой для discovery (страница тега/профиля) и экстракции
    /// (страница `/video/<id>`). Классифицирует отказы так, чтобы пул прокси и
    /// ретраи действовали осмысленно: region-block помечает узел плохим и
    /// уходит на прокси другого региона, бот-стена отдаёт `Parse` (выше по
    /// стеку — fallback на yt-dlp).
    pub async fn fetch_page(&self, url: &str) -> Result<Value> {
        let (client, proxy) = self.http.pick();
        let mut req = client
            .get(url)
            .header("Accept-Language", "en-US,en;q=0.9")
            .header("Referer", "https://www.tiktok.com/");
        if let Some(c) = &self.cookie {
            req = req.header("Cookie", c);
        }
        let resp = match req.send().await {
            Ok(r) => r,
            Err(e) => {
                self.http.report(&proxy, false);
                return Err(Error::Transient(format!("tiktok GET {url}: {e}")));
            }
        };

        let status = resp.status();
        if status.as_u16() == 429 {
            self.http.report(&proxy, false);
            return Err(Error::RateLimited(format!("tiktok {url} 429")));
        }
        if !status.is_success() {
            self.http.report(&proxy, false);
            return Err(Error::Transient(format!("tiktok {url} HTTP {status}")));
        }
        let html = resp
            .text()
            .await
            .map_err(|e| Error::Transient(format!("tiktok body {url}: {e}")))?;

        // Регион прокси выпилен из TikTok — на этом узле бесполезно ретраить.
        if is_region_block(&html) {
            self.http.report(&proxy, false);
            return Err(Error::RegionBlocked(format!(
                "tiktok {url}: регион прокси заблокирован (смени узел на другой регион)"
            )));
        }

        let Some(json) = extract_embedded_json(&html) else {
            // Нет встроенного состояния — обычно бот-стена. Узел не виноват
            // (лечится куками/msToken), поэтому прокси не штрафуем.
            self.http.report(&proxy, true);
            return Err(Error::Parse(
                "в HTML нет встроенного JSON (бот-стена?)".into(),
            ));
        };
        let v: Value = serde_json::from_str(json)
            .map_err(|e| Error::Parse(format!("tiktok embedded JSON: {e}")))?;
        self.http.report(&proxy, true);
        Ok(v)
    }

    /// Скачать страницу тега/профиля и вытащить из встроенного JSON список видео.
    pub async fn discover_url(&self, url: &str, limit: usize) -> Result<Vec<TikTokItem>> {
        let v = self.fetch_page(url).await?;
        let mut items = Vec::new();
        let mut seen = HashSet::new();
        collect_items(&v, &mut items, &mut seen);
        items.truncate(limit);
        Ok(items)
    }

    /// Тир A автосбора кук (ADR-0003): GET tiktok.com и собрать анонимные куки
    /// доверия из `Set-Cookie` (`ttwid`, `tt_csrf_token`, `tt_chain_token`).
    /// БЕЗ браузера и без логина → без `msToken` (он JS-генерируемый — это Тир B).
    /// `None`, если сервер не отдал кук. Применяется, когда cookie не настроена.
    pub async fn bootstrap_cookies(&self) -> Result<Option<String>> {
        let (client, proxy) = self.http.pick();
        let resp = match client
            .get("https://www.tiktok.com/")
            .header("Accept-Language", "en-US,en;q=0.9")
            .header("Referer", "https://www.tiktok.com/")
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                self.http.report(&proxy, false);
                return Err(Error::Transient(format!("tiktok bootstrap GET: {e}")));
            }
        };
        let status = resp.status();
        if !status.is_success() {
            self.http.report(&proxy, false);
            return Err(Error::Transient(format!("tiktok bootstrap HTTP {status}")));
        }
        self.http.report(&proxy, true);

        let raw = resp
            .headers()
            .get_all(reqwest::header::SET_COOKIE)
            .iter()
            .filter_map(|hv| hv.to_str().ok());
        let jar = cookies_from_set_cookie(raw);
        Ok(jar_to_header(&jar))
    }
}

/// Собрать `name=value` из заголовков Set-Cookie (берём первый сегмент каждого,
/// до `;` — остальное это атрибуты Path/Domain/HttpOnly).
fn cookies_from_set_cookie<'a>(values: impl Iterator<Item = &'a str>) -> BTreeMap<String, String> {
    let mut jar = BTreeMap::new();
    for s in values {
        if let Some((k, v)) = s.split(';').next().and_then(|p| p.split_once('=')) {
            let (k, v) = (k.trim(), v.trim());
            if !k.is_empty() && !v.is_empty() {
                jar.insert(k.to_string(), v.to_string());
            }
        }
    }
    jar
}

/// Склеить jar в строку заголовка `Cookie`. `None`, если пусто.
fn jar_to_header(jar: &BTreeMap<String, String>) -> Option<String> {
    if jar.is_empty() {
        return None;
    }
    Some(jar.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("; "))
}

/// Достать содержимое `<script id="...">…</script>` для известных маркеров.
fn extract_embedded_json(html: &str) -> Option<&str> {
    for marker in ["__UNIVERSAL_DATA_FOR_REHYDRATION__", "SIGI_STATE"] {
        if let Some(pos) = html.find(marker) {
            let after = &html[pos..];
            // '>' закрывает открывающий тег <script ...>, дальше — JSON до </script>
            if let Some(gt) = after.find('>') {
                let rest = &after[gt + 1..];
                if let Some(end) = rest.find("</script>") {
                    let body = rest[..end].trim();
                    if body.starts_with('{') {
                        return Some(body);
                    }
                }
            }
        }
    }
    None
}

/// Признак того, что TikTok отдал страницу-заглушку «ушли из региона» —
/// это привязано к региону прокси, а не к нашему запросу.
fn is_region_block(html: &str) -> bool {
    html.contains("We regret to inform you that we have discontinued operating TikTok")
}

/// Числовой ли id (TikTok video id — длинная строка цифр).
fn is_video_id(s: &str) -> bool {
    s.len() >= 6 && s.bytes().all(|b| b.is_ascii_digit())
}

/// Имя автора из поля `author` (строка или объект с `uniqueId`).
fn author_username(item: &serde_json::Map<String, Value>) -> Option<String> {
    match item.get("author") {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Object(a)) => a.get("uniqueId").and_then(|u| u.as_str()).map(str::to_owned),
        _ => None,
    }
}

/// Рекурсивно собрать video items: объект с числовым `id` и признаком видео
/// (`desc`/`video`/`stats`). Устойчиво к изменениям раскладки.
fn collect_items(v: &Value, out: &mut Vec<TikTokItem>, seen: &mut HashSet<String>) {
    match v {
        Value::Object(map) => {
            if let Some(Value::String(id)) = map.get("id") {
                let looks_video = map.contains_key("desc")
                    || map.contains_key("video")
                    || map.contains_key("stats");
                if looks_video && is_video_id(id) && seen.insert(id.clone()) {
                    out.push(TikTokItem { id: id.clone(), username: author_username(map) });
                }
            }
            for val in map.values() {
                collect_items(val, out, seen);
            }
        }
        Value::Array(arr) => {
            for val in arr {
                collect_items(val, out, seen);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_and_collects_items_from_embedded_html() {
        // мини-имитация TikTok-страницы со встроенным состоянием
        let html = r#"<html><head>
        <script id="__UNIVERSAL_DATA_FOR_REHYDRATION__" type="application/json">
        {"__DEFAULT_SCOPE__":{"webapp.challenge-detail":{
          "itemList":[
            {"id":"7300000000000000001","desc":"a","author":{"uniqueId":"alice"},"stats":{"playCount":10}},
            {"id":"7300000000000000002","desc":"b","author":"bob"}
          ]},
          "seo.abtest":{"id":"not-a-video"}
        }}
        </script></head><body></body></html>"#;

        let json = extract_embedded_json(html).expect("embedded json found");
        let v: Value = serde_json::from_str(json).unwrap();
        let mut items = Vec::new();
        let mut seen = HashSet::new();
        collect_items(&v, &mut items, &mut seen);

        assert_eq!(items.len(), 2, "ровно два видео (seo.abtest отброшен)");
        assert_eq!(items[0].id, "7300000000000000001");
        assert_eq!(items[0].username.as_deref(), Some("alice"));
        assert_eq!(items[1].username.as_deref(), Some("bob"));
    }

    #[test]
    fn no_embedded_json_returns_none() {
        assert!(extract_embedded_json("<html>bot wall</html>").is_none());
    }

    #[test]
    fn parses_anonymous_cookies_from_set_cookie() {
        let vals = [
            "ttwid=abc123; Path=/; Domain=.tiktok.com; HttpOnly; Secure",
            "tt_csrf_token=XYZ789; Path=/; Secure",
            "tt_chain_token=QQ; Path=/",
            "  ; junk-without-eq",
            "empty=; Path=/",
        ];
        let jar = cookies_from_set_cookie(vals.iter().copied());
        assert_eq!(jar.get("ttwid").map(String::as_str), Some("abc123"));
        assert_eq!(jar.get("tt_csrf_token").map(String::as_str), Some("XYZ789"));
        assert_eq!(jar.get("tt_chain_token").map(String::as_str), Some("QQ"));
        assert!(!jar.contains_key("empty"), "пустое значение отброшено");
        assert_eq!(jar.len(), 3);

        let header = jar_to_header(&jar).unwrap();
        // BTreeMap → детерминированный порядок по имени
        assert_eq!(header, "tt_chain_token=QQ; tt_csrf_token=XYZ789; ttwid=abc123");
        assert!(jar_to_header(&BTreeMap::new()).is_none());
    }

    #[test]
    fn detects_region_block_page() {
        let blocked = "<html><body><p>\n  We regret to inform you that we have \
                       discontinued operating TikTok in your region.\n</p></body></html>";
        assert!(is_region_block(blocked));
        assert!(!is_region_block("<html>normal page</html>"));
    }
}
