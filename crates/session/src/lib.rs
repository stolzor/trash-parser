//! Подсистема прогретых сессий для TikTok-пути (ADR-0002).
//!
//! Единица работы — прогретая **идентичность** (`sticky_proxy + fingerprint +
//! cookie_jar + lifecycle`), а не «прокси на запрос». Это поднимает «пол»
//! надёжности против сессионного скоринга TikTok.
//!
//! Этот крейт — **clock-free ядро**: модель ([`identity`]), пул ([`pool`]) и
//! персистентность ([`store`]). Тяжёлые части (браузерный прогрев, боевой
//! TLS-impersonation-клиент) подключаются за трейтами отдельным инкрементом —
//! см. открытые вопросы ADR-0002.

pub mod identity;
pub mod pool;
pub mod store;
pub mod warmer;

pub use identity::{CookieJar, Fingerprint, HealthPolicy, Identity, Lifecycle, Outcome};
pub use pool::SessionPool;
pub use store::{SessionStore, SqliteSessionStore};
pub use warmer::Warmer;
