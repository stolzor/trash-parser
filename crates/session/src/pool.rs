//! Пул прогретых идентичностей. Раздаёт `Ready`-идентичность на запрос
//! (round-robin среди здоровых), учитывает исходы и подсказывает воркеру, кого
//! пора прогреть/освежить.
//!
//! Как и [`identity`], модуль clock-free: `now` инжектится. В отличие от
//! `ProxyPool`, `acquire` возвращает `None`, когда ни одна идентичность не
//! готова — это осмысленный сигнал «деградируй на холодный путь», а не повод
//! долбить непрогретую сессию.

use crate::identity::{CookieJar, HealthPolicy, Identity, Lifecycle, Outcome};
use std::sync::Mutex;

struct Inner {
    ids: Vec<Identity>,
    /// Курсор round-robin.
    next: usize,
}

pub struct SessionPool {
    inner: Mutex<Inner>,
    policy: HealthPolicy,
}

impl SessionPool {
    pub fn new(policy: HealthPolicy) -> Self {
        Self { inner: Mutex::new(Inner { ids: Vec::new(), next: 0 }), policy }
    }

    /// Поднять пул из ранее сохранённых идентичностей (из session store).
    pub fn from_identities(ids: Vec<Identity>, policy: HealthPolicy) -> Self {
        Self { inner: Mutex::new(Inner { ids, next: 0 }), policy }
    }

    /// Добавить идентичность (обычно `Fresh`, далее её подхватит воркер прогрева).
    pub fn register(&self, identity: Identity) {
        self.inner.lock().unwrap().ids.push(identity);
    }

    pub fn total(&self) -> usize {
        self.inner.lock().unwrap().ids.len()
    }

    /// Сколько идентичностей готовы прямо сейчас (прогреты и не в cooldown).
    pub fn ready_count(&self, now: i64) -> usize {
        let g = self.inner.lock().unwrap();
        g.ids.iter().filter(|i| i.is_available(now)).count()
    }

    /// Выдать готовую идентичность (round-robin среди доступных). `None`, если
    /// ни одна не готова — вызывающий деградирует на холодный путь / yt-dlp.
    pub fn acquire(&self, now: i64) -> Option<Identity> {
        let mut g = self.inner.lock().unwrap();
        let n = g.ids.len();
        if n == 0 {
            return None;
        }
        for off in 0..n {
            let i = (g.next + off) % n;
            if g.ids[i].is_available(now) {
                g.next = (i + 1) % n;
                return Some(g.ids[i].clone());
            }
        }
        None
    }

    /// Учесть исход боевого запроса для идентичности.
    pub fn record(&self, id: &str, outcome: Outcome, now: i64) {
        let mut g = self.inner.lock().unwrap();
        let policy = self.policy;
        if let Some(it) = g.ids.iter_mut().find(|i| i.id == id) {
            it.record(outcome, now, &policy);
        }
    }

    /// Пометить, что воркер начал прогрев идентичности.
    pub fn mark_warming(&self, id: &str) {
        let mut g = self.inner.lock().unwrap();
        if let Some(it) = g.ids.iter_mut().find(|i| i.id == id) {
            it.mark_warming();
        }
    }

    /// Завершить прогрев: принять jar и перевести в `Ready`.
    pub fn mark_warmed(&self, id: &str, jar: CookieJar, now: i64) {
        let mut g = self.inner.lock().unwrap();
        if let Some(it) = g.ids.iter_mut().find(|i| i.id == id) {
            it.mark_warmed(jar, now);
        }
    }

    /// id'ы, которым нужен прогрев/refresh (для воркера). `Fresh` + протухшие.
    pub fn needs_warming(&self, now: i64, max_age: i64) -> Vec<String> {
        let g = self.inner.lock().unwrap();
        g.ids
            .iter()
            .filter(|i| i.needs_warming(now, max_age))
            .map(|i| i.id.clone())
            .collect()
    }

    /// Снять сожжённые идентичности из пула и вернуть их (для удаления из store
    /// и освобождения sticky-прокси под новую идентичность).
    pub fn retire_burned(&self) -> Vec<Identity> {
        let mut g = self.inner.lock().unwrap();
        let mut burned = Vec::new();
        let mut kept = Vec::with_capacity(g.ids.len());
        for it in g.ids.drain(..) {
            if it.lifecycle == Lifecycle::Burned {
                burned.push(it);
            } else {
                kept.push(it);
            }
        }
        g.ids = kept;
        if g.next >= g.ids.len().max(1) {
            g.next = 0;
        }
        burned
    }

    /// Копия идентичности по id (для сохранения в store после изменения).
    pub fn snapshot(&self, id: &str) -> Option<Identity> {
        let g = self.inner.lock().unwrap();
        g.ids.iter().find(|i| i.id == id).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::Fingerprint;

    fn fp() -> Fingerprint {
        Fingerprint {
            user_agent: "UA".into(),
            accept_language: "en".into(),
            timezone: "UTC".into(),
            locale: "en-US".into(),
            tls_profile: "chrome_131".into(),
        }
    }

    fn fresh(id: &str) -> Identity {
        Identity::new(id, format!("http://{id}@h:1"), fp(), 0)
    }

    fn pool_with_ready(ids: &[&str]) -> SessionPool {
        let p = SessionPool::new(HealthPolicy::default());
        for id in ids {
            let mut it = fresh(id);
            it.mark_warmed(CookieJar::new(), 0);
            p.register(it);
        }
        p
    }

    #[test]
    fn acquire_round_robins_ready_only() {
        let p = pool_with_ready(&["a", "b"]);
        p.register(fresh("c")); // Fresh — не должна выдаваться
        assert_eq!(p.acquire(0).map(|i| i.id), Some("a".into()));
        assert_eq!(p.acquire(0).map(|i| i.id), Some("b".into()));
        assert_eq!(p.acquire(0).map(|i| i.id), Some("a".into()), "c пропущена (Fresh)");
    }

    #[test]
    fn empty_or_none_ready_returns_none() {
        let p = SessionPool::new(HealthPolicy::default());
        assert!(p.acquire(0).is_none(), "пустой пул");
        p.register(fresh("a")); // только Fresh
        assert!(p.acquire(0).is_none(), "нет ни одной Ready");
    }

    #[test]
    fn cooling_identity_is_skipped_then_returns() {
        let p = pool_with_ready(&["a"]);
        p.record("a", Outcome::Soft, 0); // cooldown до 30
        assert!(p.acquire(10).is_none(), "в cooldown");
        assert_eq!(p.acquire(30).map(|i| i.id), Some("a".into()));
    }

    #[test]
    fn burned_identity_is_retired() {
        let p = pool_with_ready(&["a", "b"]);
        p.record("a", Outcome::Hard, 0); // сжечь a
        assert_eq!(p.ready_count(0), 1);
        let burned = p.retire_burned();
        assert_eq!(burned.len(), 1);
        assert_eq!(burned[0].id, "a");
        assert_eq!(p.total(), 1);
        // курсор не выходит за границы после ретайра
        assert_eq!(p.acquire(0).map(|i| i.id), Some("b".into()));
    }

    #[test]
    fn needs_warming_lists_fresh_and_stale() {
        let p = SessionPool::new(HealthPolicy::default());
        p.register(fresh("fresh")); // Fresh
        let mut ready = fresh("ready");
        ready.mark_warmed(CookieJar::new(), 1000);
        p.register(ready);
        // на t=1100, max_age=3600: свежепрогретая не нужна, Fresh — нужна
        assert_eq!(p.needs_warming(1100, 3600), vec!["fresh".to_string()]);
        // на t=9000 протухает и "ready"
        let mut due = p.needs_warming(9000, 3600);
        due.sort();
        assert_eq!(due, vec!["fresh".to_string(), "ready".to_string()]);
    }
}
