//! Host-owned completion delivery. The inbox is shared by model-request notices
//! and idle wake admission; a signal never consumes a completion by itself.
use anyhow::Result;
use bubblepaw_core::{
    CancellationToken, Harness, MessageOrigin, Reminder, Run, RunOptions, SessionBusy, Store,
};
use std::{
    collections::{BTreeMap, HashSet},
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::Notify;

pub type WakeReminderCallback = Arc<dyn Fn(Vec<Reminder>) -> Result<Run> + Send + Sync>;
pub type WakeReminderFallback = Arc<dyn Fn(&str, Vec<Reminder>) -> Result<Run> + Send + Sync>;

#[path = "wake_recovery.rs"]
mod recovery;
pub(crate) use recovery::{DeliveryMarker, record_delivers, undelivered_markers};
#[path = "wake_delivery.rs"]
mod delivery;

struct Notice {
    reminder: Reminder,
    at: Instant,
    required: bool,
}
#[derive(Default)]
struct Inbox {
    notices: BTreeMap<String, BTreeMap<String, Notice>>,
    suppressed: HashSet<String>,
    callbacks: BTreeMap<String, (u64, WakeReminderCallback)>,
    fallback: Option<(u64, WakeReminderFallback)>,
    generation: u64,
}

/// Registrations and the driver belong to the embedding host's lifetime.
/// Callbacks must synchronously admit a run, must not call this coordinator,
/// and must attach any host-specific observation/cleanup before returning.
/// Admission holds the inbox lock so a model request cannot concurrently consume
/// the same notices. Failed admission leaves the inbox intact.
pub struct WakeCoordinator {
    enabled: AtomicBool,
    debounce_ms: AtomicU64,
    inbox: Mutex<Inbox>,
    retired: Mutex<Vec<(Option<String>, u64)>>,
    changed: Notify,
}
impl Default for WakeCoordinator {
    fn default() -> Self {
        Self::new(true, Duration::from_millis(1500))
    }
}
impl WakeCoordinator {
    pub fn new(enabled: bool, debounce: Duration) -> Self {
        Self {
            enabled: AtomicBool::new(enabled),
            debounce_ms: AtomicU64::new(debounce.as_millis().min(u64::MAX as u128) as u64),
            inbox: Mutex::default(),
            retired: Mutex::default(),
            changed: Notify::new(),
        }
    }
    pub fn configure(&self, enabled: bool, debounce: Duration) {
        self.enabled.store(enabled, Ordering::Relaxed);
        self.debounce_ms.store(
            debounce.as_millis().min(u64::MAX as u128) as u64,
            Ordering::Relaxed,
        );
        self.changed.notify_one();
    }
    pub fn publish(&self, owner: &str, source: impl Into<String>, text: String) {
        self.publish_if(owner, source, text, || true);
    }
    /// A blocked child still needs an answer when optional completion wakes are disabled.
    pub fn publish_required(&self, owner: &str, source: impl Into<String>, text: String) {
        let source = source.into();
        self.publish_reminder_required(
            owner,
            source.clone(),
            Reminder::new(format!("notice:{source}"), text),
        );
    }
    pub(crate) fn publish_if(
        &self,
        owner: &str,
        source: impl Into<String>,
        text: String,
        valid: impl FnOnce() -> bool,
    ) {
        let source = source.into();
        self.publish_reminder_if(
            owner,
            source.clone(),
            Reminder::new(format!("notice:{source}"), text),
            valid,
        );
    }
    pub(crate) fn publish_reminder_required(
        &self,
        owner: &str,
        source: impl Into<String>,
        reminder: Reminder,
    ) {
        self.publish_policy_if(owner, source, reminder, true, || true);
    }
    pub(crate) fn publish_reminder_if(
        &self,
        owner: &str,
        source: impl Into<String>,
        reminder: Reminder,
        valid: impl FnOnce() -> bool,
    ) {
        self.publish_policy_if(owner, source, reminder, false, valid);
    }
    fn publish_policy_if(
        &self,
        owner: &str,
        source: impl Into<String>,
        reminder: Reminder,
        required: bool,
        valid: impl FnOnce() -> bool,
    ) {
        let mut inbox = self.inbox.lock().expect("wake inbox poisoned");
        if valid() {
            inbox.notices.entry(owner.into()).or_default().insert(
                source.into(),
                Notice {
                    reminder,
                    at: Instant::now(),
                    required,
                },
            );
        }
        drop(inbox);
        self.changed.notify_one();
    }
    pub fn remove(&self, owner: &str, source: &str) {
        if let Ok(mut inbox) = self.inbox.lock()
            && let Some(notices) = inbox.notices.get_mut(owner)
        {
            notices.remove(source);
        }
        self.changed.notify_one();
    }
    pub(crate) fn consume<T>(
        &self,
        owner: &str,
        read: impl FnOnce() -> Result<(T, Option<String>)>,
    ) -> Result<T> {
        let mut inbox = self.inbox.lock().expect("wake inbox poisoned");
        let (value, source) = read()?;
        if let Some(source) = source
            && let Some(notices) = inbox.notices.get_mut(owner)
        {
            notices.remove(&source);
        }
        Ok(value)
    }
    /// Consume only notices owned by this tool family (e.g. `process:`).
    pub fn take_reminders(&self, owner: &str, prefix: &str) -> Vec<Reminder> {
        let mut inbox = self.inbox.lock().expect("wake inbox poisoned");
        let Some(notices) = inbox.notices.get_mut(owner) else {
            return Vec::new();
        };
        let keys = notices
            .keys()
            .filter(|key| key.starts_with(prefix))
            .cloned()
            .collect::<Vec<_>>();
        keys.into_iter()
            .filter_map(|key| notices.remove(&key).map(|notice| notice.reminder))
            .collect()
    }
    pub fn discard(&self, owner: &str, prefix: &str) {
        let _ = self.take_reminders(owner, prefix);
    }
    /// Explicit stop/close persists in this host until accepted user text.
    pub fn suppress(&self, owner: &str) {
        self.inbox
            .lock()
            .expect("wake inbox poisoned")
            .suppressed
            .insert(owner.into());
        self.changed.notify_one();
    }
    pub fn user_input(&self, owner: &str) {
        self.inbox
            .lock()
            .expect("wake inbox poisoned")
            .suppressed
            .remove(owner);
        self.changed.notify_one();
    }
    pub fn register_reminders(
        self: &Arc<Self>,
        owner: impl Into<String>,
        callback: WakeReminderCallback,
    ) -> WakeRegistration {
        let owner = owner.into();
        let mut inbox = self.inbox.lock().expect("wake inbox poisoned");
        inbox.generation += 1;
        let generation = inbox.generation;
        inbox
            .callbacks
            .insert(owner.clone(), (generation, callback));
        drop(inbox);
        self.changed.notify_one();
        WakeRegistration {
            coordinator: Arc::downgrade(self),
            owner: Some(owner),
            generation,
        }
    }
    pub fn register_reminder_fallback(
        self: &Arc<Self>,
        callback: WakeReminderFallback,
    ) -> WakeRegistration {
        let mut inbox = self.inbox.lock().expect("wake inbox poisoned");
        inbox.generation += 1;
        let generation = inbox.generation;
        inbox.fallback = Some((generation, callback));
        drop(inbox);
        self.changed.notify_one();
        WakeRegistration {
            coordinator: Arc::downgrade(self),
            owner: None,
            generation,
        }
    }
    /// A root fallback for hosts whose normal event subscription owns output.
    /// Visible child registrations override it and retain child lifecycle logic.
    pub fn register_harness(self: &Arc<Self>, harness: Arc<Harness>) -> WakeRegistration {
        self.register_harness_scoped(harness, crate::session_scope::SessionScope::all())
    }
    pub fn register_harness_scoped(
        self: &Arc<Self>,
        harness: Arc<Harness>,
        scope: crate::session_scope::SessionScope,
    ) -> WakeRegistration {
        let harness = Arc::downgrade(&harness);
        self.register_reminder_fallback(Arc::new(move |owner, reminders| {
            let harness = harness
                .upgrade()
                .ok_or_else(|| anyhow::anyhow!("root host was released"))?;
            let session = harness
                .store()
                .active_session_metadata(owner)?
                .ok_or_else(|| anyhow::anyhow!("session is closed or archived"))?;
            anyhow::ensure!(
                !session.hidden && !session.read_only && session.parent_session_id.is_none(),
                "session is not an active visible root"
            );
            scope.admit_owned(harness.store(), owner, || {
                harness.start_reminders_if_active(
                    owner,
                    reminders,
                    RunOptions {
                        origin: MessageOrigin::Generated,
                        ..Default::default()
                    },
                )
            })
        }))
    }
    pub fn start_driver(self: &Arc<Self>) -> WakeDriver {
        let stop = CancellationToken::new();
        let stopped = stop.clone();
        let coordinator = self.clone();
        let task = tokio::spawn(async move {
            loop {
                let changed = coordinator.changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                let delay = coordinator.deliver_ready();
                tokio::select! {
                    _ = stopped.cancelled() => break,
                    _ = changed => {},
                    _ = async { match delay { Some(delay) => tokio::time::sleep(delay).await, None => std::future::pending::<()>().await } } => {},
                }
            }
        });
        WakeDriver { stop, task }
    }
}
pub struct WakeRegistration {
    coordinator: Weak<WakeCoordinator>,
    owner: Option<String>,
    generation: u64,
}
impl Drop for WakeRegistration {
    fn drop(&mut self) {
        if let Some(coordinator) = self.coordinator.upgrade() {
            // A child may be evicted while its registry lock is held. Queue
            // retirement separately to preserve inbox -> registry lock order.
            coordinator
                .retired
                .lock()
                .expect("wake retirement lock")
                .push((self.owner.clone(), self.generation));
            coordinator.changed.notify_one();
        }
    }
}
pub struct WakeDriver {
    stop: CancellationToken,
    task: tokio::task::JoinHandle<()>,
}
impl WakeDriver {
    pub async fn shutdown(mut self) {
        self.stop.cancel();
        self.task.abort();
        let _ = (&mut self.task).await;
    }
}
impl Drop for WakeDriver {
    fn drop(&mut self) {
        self.stop.cancel();
        self.task.abort();
    }
}
