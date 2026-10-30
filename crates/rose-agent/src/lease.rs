//! Control lease: exactly one driver — Rose or the human.
//!
//! The lease only ever *pauses* the agent loop. Taking control never kills
//! anything: the loop waits at its next checkpoint while a human holds the
//! lease and resumes on hand-back, forcing a fresh observation first (the
//! desktop may have changed under it). Human input rides the same
//! `DesktopBackend::act` path with `Actor::Human`, so there is one input
//! channel and one audit log.

use std::sync::{Arc, RwLock};
use tokio::sync::Notify;

/// Who may inject input right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Driver {
    Agent,
    Human,
}

impl Driver {
    pub fn as_str(self) -> &'static str {
        match self {
            Driver::Agent => "agent",
            Driver::Human => "human",
        }
    }
}

struct Inner {
    holder: RwLock<Driver>,
    changed: Notify,
}

/// Shareable single-driver token. Starts with the agent; `Clone` shares
/// the same underlying state (composition root hands one to the web layer
/// and one to the loop).
#[derive(Clone)]
pub struct Lease {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for Lease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Lease")
            .field("holder", &self.holder())
            .finish()
    }
}

impl Default for Lease {
    fn default() -> Self {
        Self::new()
    }
}

impl Lease {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                holder: RwLock::new(Driver::Agent),
                changed: Notify::new(),
            }),
        }
    }

    /// Current holder. Lock-free read; callers re-check at action time.
    pub fn holder(&self) -> Driver {
        *self.inner.holder.read().expect("lease")
    }

    fn set(&self, driver: Driver) {
        *self.inner.holder.write().expect("lease") = driver;
        self.inner.changed.notify_waiters();
    }

    /// Human takes the desktop. The agent loop pauses at its next
    /// checkpoint; an in-flight atomic action still finishes.
    pub fn take_human(&self) {
        self.set(Driver::Human);
    }

    /// Human hands back. The loop resumes and re-observes before acting.
    pub fn hand_back(&self) {
        self.set(Driver::Agent);
    }

    /// Resolve when the holder is the agent again (hand-back or a fresh
    /// lease). Returns immediately if the agent already holds it.
    pub async fn wait_for_agent(&self) {
        loop {
            // Register interest *before* checking, so a hand-back between
            // the check and the sleep cannot be missed.
            let notified = self.inner.changed.notified();
            if self.holder() == Driver::Agent {
                return;
            }
            notified.await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_with_agent() {
        let l = Lease::new();
        assert_eq!(l.holder(), Driver::Agent);
        assert_eq!(Driver::Human.as_str(), "human");
        assert_eq!(Lease::default().holder(), Driver::Agent);
    }

    #[test]
    fn takeover_and_handback_flip_holder() {
        let l = Lease::new();
        l.take_human();
        assert_eq!(l.holder(), Driver::Human);
        l.hand_back();
        assert_eq!(l.holder(), Driver::Agent);
    }

    #[test]
    fn clones_share_state() {
        let a = Lease::new();
        let b = a.clone();
        a.take_human();
        assert_eq!(b.holder(), Driver::Human);
        b.hand_back();
        assert_eq!(a.holder(), Driver::Agent);
    }

    #[tokio::test]
    async fn wait_resolves_immediately_when_agent_holds_it() {
        let l = Lease::new();
        tokio::time::timeout(std::time::Duration::from_secs(2), l.wait_for_agent())
            .await
            .expect("must not block");
    }

    #[tokio::test]
    async fn wait_unblocks_on_handback() {
        let l = Lease::new();
        l.take_human();
        let l2 = l.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            l2.hand_back();
        });
        tokio::time::timeout(std::time::Duration::from_secs(2), l.wait_for_agent())
            .await
            .expect("handback must wake the waiter");
        assert_eq!(l.holder(), Driver::Agent);
    }
}
