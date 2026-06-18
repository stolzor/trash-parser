//! HTTP-клиент с ротацией прокси поверх `ProxyPool`. Кэширует по одному
//! `reqwest::Client` на прокси (создание клиента с TLS дорого). Используется
//! нативными discovery-клиентами (YouTube InnerTube, TikTok web).

use detox_parser_core::ProxyPool;
use reqwest::Client;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tracing::warn;

pub struct ProxyHttp {
    pool: Option<Arc<ProxyPool>>,
    ua: String,
    default: Client,
    /// Клиенты по url прокси (ленивое кэширование).
    cache: Mutex<HashMap<String, Client>>,
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn build_client(ua: &str, proxy: Option<&str>) -> reqwest::Result<Client> {
    let mut b = Client::builder().user_agent(ua);
    if let Some(p) = proxy {
        b = b.proxy(reqwest::Proxy::all(p)?);
    }
    b.build()
}

impl ProxyHttp {
    pub fn new(pool: Option<Arc<ProxyPool>>, ua: impl Into<String>) -> Self {
        let ua = ua.into();
        let default = build_client(&ua, None).expect("default reqwest client");
        Self { pool, ua, default, cache: Mutex::new(HashMap::new()) }
    }

    /// Выбрать клиента (через ротацию прокси) + url прокси для последующего
    /// отчёта. Без пула / при ошибке сборки прокси-клиента — дефолтный клиент.
    pub fn pick(&self) -> (Client, Option<String>) {
        let Some(px) = self.pool.as_ref().and_then(|p| p.acquire(now_unix())) else {
            return (self.default.clone(), None);
        };
        let mut cache = self.cache.lock().unwrap();
        if let Some(c) = cache.get(&px) {
            return (c.clone(), Some(px));
        }
        match build_client(&self.ua, Some(&px)) {
            Ok(c) => {
                cache.insert(px.clone(), c.clone());
                (c, Some(px))
            }
            Err(e) => {
                warn!(proxy = %px, error = %e, "плохой прокси-url → дефолтный клиент");
                (self.default.clone(), None)
            }
        }
    }

    /// Сообщить пулу результат запроса через выбранный прокси.
    pub fn report(&self, proxy: &Option<String>, ok: bool) {
        if let (Some(pool), Some(px)) = (&self.pool, proxy) {
            pool.report(px, ok, now_unix());
        }
    }
}
