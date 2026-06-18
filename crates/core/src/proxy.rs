//! Пул прокси из файла с «умным» распределением: round-robin раздаёт нагрузку
//! между конкурентными запросами, а сбойные прокси уходят в экспоненциальный
//! cooldown и пропускаются (чтобы не долбить забаненный/мёртвый прокси).
//!
//! Логика без часов и без HTTP: `now` передаётся снаружи (как и везде в ядре),
//! а конкретные клиенты (yt-dlp/reqwest) живут в своих крейтах.

use crate::error::{Error, Result};
use std::path::Path;
use std::sync::Mutex;

/// Максимальная степень экспоненты cooldown (cooldown ≤ base·2^6).
const MAX_BACKOFF_EXP: u32 = 6;

#[derive(Debug)]
struct ProxyState {
    url: String,
    /// Прокси недоступен до этого unix-времени (0 = доступен).
    cooldown_until: i64,
    /// Подряд идущих сбоев (для экспоненты).
    fails: u32,
}

struct Inner {
    proxies: Vec<ProxyState>,
    next: usize,
}

pub struct ProxyPool {
    inner: Mutex<Inner>,
    cooldown_base: i64,
}

impl ProxyPool {
    /// Список прокси, по одному URL на строку. Пустые строки и `#`-комментарии
    /// игнорируются. Формат строки — любой, что понимают yt-dlp/reqwest:
    /// `http://[user:pass@]host:port`, `socks5://host:port`.
    pub fn from_list(urls: Vec<String>, cooldown_base_secs: u64) -> Self {
        let proxies = urls
            .into_iter()
            .map(|url| ProxyState { url, cooldown_until: 0, fails: 0 })
            .collect();
        Self {
            inner: Mutex::new(Inner { proxies, next: 0 }),
            cooldown_base: cooldown_base_secs.max(1) as i64,
        }
    }

    pub fn from_file(path: impl AsRef<Path>, cooldown_base_secs: u64) -> Result<Self> {
        let text = std::fs::read_to_string(path.as_ref())
            .map_err(|e| Error::Config(format!("не прочитать proxy-файл: {e}")))?;
        let urls: Vec<String> = text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .map(str::to_owned)
            .collect();
        if urls.is_empty() {
            return Err(Error::Config("proxy-файл пуст".into()));
        }
        Ok(Self::from_list(urls, cooldown_base_secs))
    }

    pub fn len(&self) -> usize {
        self.inner.lock().unwrap().proxies.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Выдать следующий прокси (round-robin среди доступных). Если все в
    /// cooldown — вернуть тот, что освободится раньше всех (не стопаем работу).
    /// `None` только если пул пуст.
    pub fn acquire(&self, now: i64) -> Option<String> {
        let mut g = self.inner.lock().unwrap();
        let n = g.proxies.len();
        if n == 0 {
            return None;
        }
        // round-robin поиск доступного
        for off in 0..n {
            let i = (g.next + off) % n;
            if g.proxies[i].cooldown_until <= now {
                g.next = (i + 1) % n;
                return Some(g.proxies[i].url.clone());
            }
        }
        // все в cooldown → берём с минимальным временем готовности
        let i = (0..n).min_by_key(|&i| g.proxies[i].cooldown_until).unwrap();
        g.next = (i + 1) % n;
        Some(g.proxies[i].url.clone())
    }

    /// Сообщить результат использования прокси. Успех сбрасывает cooldown,
    /// сбой наращивает экспоненциальный cooldown.
    pub fn report(&self, url: &str, ok: bool, now: i64) {
        let mut g = self.inner.lock().unwrap();
        let base = self.cooldown_base;
        if let Some(p) = g.proxies.iter_mut().find(|p| p.url == url) {
            if ok {
                p.fails = 0;
                p.cooldown_until = 0;
            } else {
                p.fails += 1;
                let exp = (p.fails - 1).min(MAX_BACKOFF_EXP);
                p.cooldown_until = now + base * (1i64 << exp);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_robin_spreads_load() {
        let p = ProxyPool::from_list(vec!["a".into(), "b".into(), "c".into()], 30);
        assert_eq!(p.acquire(0).as_deref(), Some("a"));
        assert_eq!(p.acquire(0).as_deref(), Some("b"));
        assert_eq!(p.acquire(0).as_deref(), Some("c"));
        assert_eq!(p.acquire(0).as_deref(), Some("a"));
    }

    #[test]
    fn failed_proxy_goes_on_cooldown_and_is_skipped() {
        let p = ProxyPool::from_list(vec!["a".into(), "b".into()], 30);
        // "a" сбоит на t=0 → cooldown до 30
        let first = p.acquire(0).unwrap();
        assert_eq!(first, "a");
        p.report("a", false, 0);
        // на t=1 acquire должен пропустить "a" (в cooldown) и дать "b"
        assert_eq!(p.acquire(1).as_deref(), Some("b"));
        assert_eq!(p.acquire(1).as_deref(), Some("b"), "a всё ещё в cooldown");
        // после истечения cooldown "a" снова в ротации
        assert_eq!(p.acquire(31).as_deref(), Some("a"));
    }

    #[test]
    fn success_resets_cooldown() {
        let p = ProxyPool::from_list(vec!["a".into()], 30);
        p.report("a", false, 0); // cooldown до 30
        p.report("a", true, 5); // успех сбрасывает
        assert_eq!(p.acquire(6).as_deref(), Some("a"));
    }

    #[test]
    fn empty_pool_returns_none() {
        let p = ProxyPool::from_list(vec![], 30);
        assert!(p.acquire(0).is_none());
        assert!(p.is_empty());
    }
}
