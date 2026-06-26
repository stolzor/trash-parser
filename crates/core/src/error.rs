//! Единый тип ошибок ядра. Крейты-адаптеры мапят свои ошибки сюда.

use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    /// Видео недоступно (приватное/удалённое/гео/age-restricted) — не ретраить.
    #[error("video unavailable: {0}")]
    Unavailable(String),

    /// Временная ошибка (сеть, 5xx, таймаут) — можно ретраить с backoff.
    #[error("transient: {0}")]
    Transient(String),

    /// Превышен лимит запросов (429) — backoff обязателен.
    #[error("rate limited: {0}")]
    RateLimited(String),

    /// Регион прокси заблокирован платформой (TikTok ушёл из региона) —
    /// ретрай на том же узле бесполезен, нужен прокси другого региона.
    #[error("region blocked: {0}")]
    RegionBlocked(String),

    /// Ответ есть, но распарсить не удалось (изменилась схема платформы).
    #[error("parse error: {0}")]
    Parse(String),

    /// Внешний инструмент (yt-dlp/ffmpeg) отсутствует или упал.
    #[error("external tool error: {0}")]
    Tool(String),

    /// Ошибка хранилища/манифеста.
    #[error("storage error: {0}")]
    Storage(String),

    /// Неверная конфигурация.
    #[error("config error: {0}")]
    Config(String),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

impl Error {
    /// Стоит ли ретраить ошибку. Регион-блок ретраим, потому что ретрай
    /// заново выбирает прокси из пула (узел-нарушитель уже помечен плохим).
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            Error::Transient(_) | Error::RateLimited(_) | Error::RegionBlocked(_)
        )
    }
}
