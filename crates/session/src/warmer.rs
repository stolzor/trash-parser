//! Прогрев идентичности — генерация `msToken`/`ttwid` и лёгкая человекоподобная
//! навигация. По ADR-0002 боевая реализация — headless Chromium (`chromiumoxide`),
//! запускается **изредка на идентичность** (первичный прогрев + периодический
//! refresh), а не на каждый запрос.
//!
//! Здесь — только граница-трейт. Браузерная реализация (`BrowserWarmer`) и выбор
//! TLS-клиента для боевого реплея относятся к открытым вопросам ADR-0002 и придут
//! отдельным инкрементом; трейт держит ядро (pool/store) от этого независимым.

use crate::identity::{CookieJar, Identity};
use async_trait::async_trait;
use detox_parser_core::error::Result;

/// Прогревает идентичность и возвращает свежий cookie-jar. Реализация обязана
/// соблюдать инвариант №1: ходить строго через `identity.proxy` с
/// `identity.fingerprint` — иначе jar окажется привязан к чужому IP/отпечатку.
#[async_trait]
pub trait Warmer: Send + Sync {
    async fn warm(&self, identity: &Identity) -> Result<CookieJar>;

    /// Освежить уже прогретую идентичность. По умолчанию — полный прогрев.
    async fn refresh(&self, identity: &Identity) -> Result<CookieJar> {
        self.warm(identity).await
    }
}

/// Заглушка для тестов/локальной отладки оркестрации без браузера: отдаёт
/// заранее заданный jar. В бою не используется.
pub struct FixedWarmer {
    pub jar: CookieJar,
}

#[async_trait]
impl Warmer for FixedWarmer {
    async fn warm(&self, _identity: &Identity) -> Result<CookieJar> {
        Ok(self.jar.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::Fingerprint;

    #[tokio::test]
    async fn fixed_warmer_returns_preset_jar() {
        let mut jar = CookieJar::new();
        jar.set("ttwid", "warmed");
        let w = FixedWarmer { jar };
        let fp = Fingerprint {
            user_agent: "UA".into(),
            accept_language: "en".into(),
            timezone: "UTC".into(),
            locale: "en-US".into(),
            tls_profile: "chrome_131".into(),
        };
        let id = Identity::new("a", "http://sticky@h:1", fp, 0);
        let out = w.warm(&id).await.unwrap();
        assert_eq!(out.get("ttwid"), Some("warmed"));
    }
}
