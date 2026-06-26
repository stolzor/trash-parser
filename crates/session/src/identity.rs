//! Прогретая идентичность — единица работы TikTok-пути вместо «прокси на запрос».
//!
//! Инвариант №1 (ADR-0002): `IP + fingerprint + cookies` согласованы и стабильны
//! на всю жизнь идентичности. Рассогласование палит сильнее холодного запроса.
//!
//! Этот модуль — чистая логика жизненного цикла и здоровья: **без часов и без
//! IO**, `now` инжектится снаружи (как и везде в ядре). Браузер/HTTP/SQLite
//! живут за трейтами в `warmer`/`store`.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Стадия жизни идентичности. «Cooling» отдельным состоянием не вводим — это
/// `Ready` с `cooldown_until > now` (по образцу cooldown'а `ProxyPool`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Lifecycle {
    /// Создана, прокси+fingerprint назначены, но ещё не прогрета (пустой jar).
    Fresh,
    /// Браузерный воркер сейчас её прогревает.
    Warming,
    /// Прогрета, есть валидные куки — годна к боевым запросам.
    Ready,
    /// Сожжена (капча/логин-стена/серия отказов/смена exit-IP) — выводится из пула.
    Burned,
}

/// Транспортный профиль идентичности (слои L1/L2 из ADR-0003): TLS/UA/locale.
/// Все поля стабильны на всю жизнь идентичности — менять под фиксированным
/// IP/jar нельзя (инвариант №1).
///
/// ВАЖНО (ADR-0003): это НЕ полный fingerprint. JS-поверхность устройства (L3:
/// WebGL/canvas/`navigator.*`) и подписи (L4: msToken/X-Bogus/a_bogus) задаёт
/// прогревающий браузер. Эти строки обязаны быть **наблюдены** с реального
/// браузера warmer'а и согласованы с тем, что видит JS TikTok, — произвольные
/// значения = рассинхрон = детект.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fingerprint {
    pub user_agent: String,
    pub accept_language: String,
    /// IANA-таймзона, напр. "America/New_York".
    pub timezone: String,
    /// Локаль, напр. "en-US".
    pub locale: String,
    /// Имя TLS/JA3-профиля боевого клиента, напр. "chrome_131". Должен
    /// соответствовать `user_agent` — иначе отпечаток рассогласован.
    pub tls_profile: String,
}

/// Cookie-jar идентичности (ttwid/msToken/tt_chain_token/…). Упорядочен для
/// детерминированного заголовка и стабильных тестов.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CookieJar {
    cookies: BTreeMap<String, String>,
}

impl CookieJar {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set(&mut self, name: impl Into<String>, value: impl Into<String>) {
        self.cookies.insert(name.into(), value.into());
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.cookies.get(name).map(String::as_str)
    }

    pub fn is_empty(&self) -> bool {
        self.cookies.is_empty()
    }

    /// Строка заголовка `Cookie`: `k1=v1; k2=v2` (порядок детерминирован).
    pub fn header(&self) -> String {
        self.cookies
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("; ")
    }
}

/// Исход боевого запроса с точки зрения здоровья идентичности (не контента).
/// Маппинг из `core::Error` — на краю, через [`Outcome::from_error`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Идентичность отработала (включая «контент недоступен» — её вины нет).
    Ok,
    /// Мягкий сбой (429/сеть/бот-стена) — наращиваем cooldown, копим серию.
    Soft,
    /// Жёсткий сигнал (капча/логин-стена/регион/смена exit-IP) — сжечь сразу.
    Hard,
}

impl Outcome {
    /// Перевести ошибку ядра в сигнал здоровья идентичности.
    pub fn from_error(err: &detox_parser_core::Error) -> Self {
        use detox_parser_core::Error::*;
        match err {
            // Sticky residential: блок региона = exit-IP мёртв для TikTok → сжечь.
            RegionBlocked(_) => Outcome::Hard,
            RateLimited(_) | Transient(_) | Parse(_) => Outcome::Soft,
            // Видео приватное/удалённое — идентичность тут ни при чём.
            Unavailable(_) => Outcome::Ok,
            _ => Outcome::Soft,
        }
    }
}

/// Политика здоровья: cooldown и порог сжигания. По образцу констант `ProxyPool`.
#[derive(Debug, Clone, Copy)]
pub struct HealthPolicy {
    /// База экспоненциального cooldown в секундах.
    pub cooldown_base: i64,
    /// Максимальная степень экспоненты (cooldown ≤ base·2^exp).
    pub max_backoff_exp: u32,
    /// Сколько мягких сбоев подряд до сжигания.
    pub burn_after_fails: u32,
}

impl Default for HealthPolicy {
    fn default() -> Self {
        Self { cooldown_base: 30, max_backoff_exp: 6, burn_after_fails: 4 }
    }
}

/// Прогретая идентичность. Сериализуется целиком в session store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Identity {
    pub id: String,
    /// Sticky residential/mobile URL со стабильным exit-IP.
    pub proxy: String,
    pub fingerprint: Fingerprint,
    pub jar: CookieJar,
    pub lifecycle: Lifecycle,
    /// Недоступна до этого unix-времени (0 = доступна). `Ready` + >now = Cooling.
    pub cooldown_until: i64,
    pub req_count: u64,
    pub consecutive_fails: u32,
    pub created_at: i64,
    /// Когда последний раз прогрета (для периодического refresh).
    pub last_warmed_at: i64,
}

impl Identity {
    /// Новая идентичность в состоянии `Fresh` (jar пуст, ждёт прогрева).
    pub fn new(
        id: impl Into<String>,
        proxy: impl Into<String>,
        fingerprint: Fingerprint,
        created_at: i64,
    ) -> Self {
        Self {
            id: id.into(),
            proxy: proxy.into(),
            fingerprint,
            jar: CookieJar::new(),
            lifecycle: Lifecycle::Fresh,
            cooldown_until: 0,
            req_count: 0,
            consecutive_fails: 0,
            created_at,
            last_warmed_at: 0,
        }
    }

    /// Годна ли к выдаче прямо сейчас: прогрета и не в cooldown.
    pub fn is_available(&self, now: i64) -> bool {
        self.lifecycle == Lifecycle::Ready && self.cooldown_until <= now
    }

    /// Пора ли освежить прогрев (куки/токены протухают). Только для `Ready`.
    pub fn is_stale(&self, now: i64, max_age: i64) -> bool {
        self.lifecycle == Lifecycle::Ready && now - self.last_warmed_at > max_age
    }

    /// Нужен ли прогрев: либо ещё ни разу (`Fresh`), либо протухла.
    pub fn needs_warming(&self, now: i64, max_age: i64) -> bool {
        self.lifecycle == Lifecycle::Fresh || self.is_stale(now, max_age)
    }

    /// Пометить, что воркер начал прогрев.
    pub fn mark_warming(&mut self) {
        if self.lifecycle != Lifecycle::Burned {
            self.lifecycle = Lifecycle::Warming;
        }
    }

    /// Завершить прогрев: принять свежий jar, перейти в `Ready`, сбросить здоровье.
    pub fn mark_warmed(&mut self, jar: CookieJar, now: i64) {
        self.jar = jar;
        self.lifecycle = Lifecycle::Ready;
        self.cooldown_until = 0;
        self.consecutive_fails = 0;
        self.last_warmed_at = now;
    }

    /// Учесть исход боевого запроса.
    pub fn record(&mut self, outcome: Outcome, now: i64, policy: &HealthPolicy) {
        match outcome {
            Outcome::Ok => {
                self.req_count += 1;
                self.consecutive_fails = 0;
                self.cooldown_until = 0;
            }
            Outcome::Soft => {
                self.req_count += 1;
                self.consecutive_fails += 1;
                if self.consecutive_fails >= policy.burn_after_fails {
                    self.lifecycle = Lifecycle::Burned;
                } else {
                    let exp = (self.consecutive_fails - 1).min(policy.max_backoff_exp);
                    self.cooldown_until = now + policy.cooldown_base * (1i64 << exp);
                }
            }
            Outcome::Hard => {
                self.lifecycle = Lifecycle::Burned;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fp() -> Fingerprint {
        Fingerprint {
            user_agent: "UA".into(),
            accept_language: "en-US,en;q=0.9".into(),
            timezone: "America/New_York".into(),
            locale: "en-US".into(),
            tls_profile: "chrome_131".into(),
        }
    }

    fn ident() -> Identity {
        Identity::new("i1", "http://sticky:1@host:1", fp(), 100)
    }

    #[test]
    fn fresh_is_not_available_until_warmed() {
        let mut id = ident();
        assert!(!id.is_available(200));
        assert!(id.needs_warming(200, 3600));
        let mut jar = CookieJar::new();
        jar.set("ttwid", "abc");
        id.mark_warmed(jar, 200);
        assert!(id.is_available(200));
        assert_eq!(id.jar.get("ttwid"), Some("abc"));
    }

    #[test]
    fn cookie_header_is_deterministic() {
        let mut jar = CookieJar::new();
        jar.set("msToken", "m");
        jar.set("ttwid", "t");
        assert_eq!(jar.header(), "msToken=m; ttwid=t");
    }

    #[test]
    fn soft_fail_sets_exponential_cooldown() {
        let mut id = ident();
        id.mark_warmed(CookieJar::new(), 0);
        let p = HealthPolicy::default(); // base 30, burn 4
        id.record(Outcome::Soft, 0, &p);
        assert_eq!(id.cooldown_until, 30, "первый сбой: base·2^0");
        assert!(!id.is_available(10));
        assert!(id.is_available(30));
        id.record(Outcome::Soft, 30, &p);
        assert_eq!(id.cooldown_until, 30 + 60, "второй сбой: base·2^1");
    }

    #[test]
    fn ok_resets_health() {
        let mut id = ident();
        id.mark_warmed(CookieJar::new(), 0);
        let p = HealthPolicy::default();
        id.record(Outcome::Soft, 0, &p);
        id.record(Outcome::Ok, 40, &p);
        assert_eq!(id.consecutive_fails, 0);
        assert_eq!(id.cooldown_until, 0);
        assert!(id.is_available(40));
    }

    #[test]
    fn burns_after_threshold_soft_fails() {
        let mut id = ident();
        id.mark_warmed(CookieJar::new(), 0);
        let p = HealthPolicy::default(); // burn_after_fails = 4
        for t in 0..4 {
            id.record(Outcome::Soft, t, &p);
        }
        assert_eq!(id.lifecycle, Lifecycle::Burned);
        assert!(!id.is_available(10_000));
    }

    #[test]
    fn hard_fail_burns_immediately() {
        let mut id = ident();
        id.mark_warmed(CookieJar::new(), 0);
        id.record(Outcome::Hard, 0, &HealthPolicy::default());
        assert_eq!(id.lifecycle, Lifecycle::Burned);
    }

    #[test]
    fn region_block_maps_to_hard() {
        let e = detox_parser_core::Error::RegionBlocked("x".into());
        assert_eq!(Outcome::from_error(&e), Outcome::Hard);
        let e = detox_parser_core::Error::RateLimited("x".into());
        assert_eq!(Outcome::from_error(&e), Outcome::Soft);
        let e = detox_parser_core::Error::Unavailable("x".into());
        assert_eq!(Outcome::from_error(&e), Outcome::Ok);
    }

    #[test]
    fn stale_ready_needs_rewarming() {
        let mut id = ident();
        id.mark_warmed(CookieJar::new(), 1000);
        assert!(!id.needs_warming(1000 + 100, 3600));
        assert!(id.needs_warming(1000 + 4000, 3600), "протухла → нужен refresh");
    }
}
