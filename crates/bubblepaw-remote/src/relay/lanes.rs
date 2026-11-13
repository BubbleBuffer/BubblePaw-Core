//! Ordered hand-off from one multiplexed connection to its sessions' queues.
//!
//! A connection carries frames for many sessions but is read by one task. When a
//! session's queue is full, its frames wait here, in order, instead of holding the
//! reader back, so the other sessions keep flowing. A lane that makes no progress
//! for the timeout is reported as stalled and dropped; one whose backlog is as
//! long as its queue holds the reader back until room appears or it stalls.
use futures_util::{StreamExt, stream::FuturesUnordered};
use std::{
    collections::{HashMap, VecDeque},
    future::Future,
    pin::Pin,
    time::Duration,
};
use tokio::{sync::mpsc, time::Instant};

/// The outcome of offering a data frame to a bounded queue.
pub(super) enum Offer<T> {
    Sent,
    Full(T),
    Gone,
}

type Room = Pin<Box<dyn Future<Output = bool> + Send>>;
type Waiting = Pin<Box<dyn Future<Output = (u64, bool)> + Send>>;

/// A bounded queue toward one session.
pub(super) trait Dest: Clone + Send + 'static {
    type Item: Send + 'static;
    /// Queue a data frame if there is room.
    fn offer(&self, item: Self::Item) -> Offer<Self::Item>;
    /// Queue a control frame regardless of room. False when the queue is gone.
    fn control(&self, item: Self::Item) -> bool;
    /// Resolves true once there may be room, false when the queue is gone.
    fn room(&self) -> Room;
}

impl Dest for mpsc::Sender<Vec<u8>> {
    type Item = Vec<u8>;
    fn offer(&self, item: Vec<u8>) -> Offer<Vec<u8>> {
        match self.try_send(item) {
            Ok(()) => Offer::Sent,
            Err(mpsc::error::TrySendError::Full(item)) => Offer::Full(item),
            Err(mpsc::error::TrySendError::Closed(_)) => Offer::Gone,
        }
    }
    fn control(&self, item: Vec<u8>) -> bool {
        self.try_send(item).is_ok()
    }
    fn room(&self) -> Room {
        let sender = self.clone();
        Box::pin(async move { sender.reserve().await.is_ok() })
    }
}

struct Lane<D: Dest> {
    dest: D,
    backlog: VecDeque<(D::Item, bool)>,
    since: Instant,
}

/// A session that made no room for the whole timeout; its lane is gone.
pub(super) struct Stalled<D> {
    pub id: u64,
    pub dest: D,
}

pub(super) struct Lanes<D: Dest> {
    lanes: HashMap<u64, Lane<D>>,
    waiting: FuturesUnordered<Waiting>,
    capacity: usize,
    timeout: Duration,
}

enum Wake {
    Room(u64, bool),
    Deadline,
}

impl<D: Dest> Lanes<D> {
    pub(super) fn new(capacity: usize, timeout: Duration) -> Self {
        Self {
            lanes: HashMap::new(),
            waiting: FuturesUnordered::new(),
            capacity,
            timeout,
        }
    }

    /// Whether some session's backlog is as long as its queue: stop reading the
    /// connection until `next` frees it or drops the session.
    pub(super) fn blocked(&self) -> bool {
        self.lanes
            .values()
            .any(|lane| lane.backlog.len() >= self.capacity)
    }

    /// Hand a frame to a session, behind any frames of its own still waiting. A
    /// control frame never waits for room.
    pub(super) fn send(&mut self, id: u64, dest: &D, item: D::Item, control: bool) {
        if let Some(lane) = self.lanes.get_mut(&id) {
            lane.backlog.push_back((item, control));
        } else if control {
            dest.control(item);
        } else if let Offer::Full(item) = dest.offer(item) {
            self.wait_for_room(id, dest);
            self.lanes.insert(
                id,
                Lane {
                    dest: dest.clone(),
                    backlog: VecDeque::from([(item, false)]),
                    since: Instant::now(),
                },
            );
        }
    }

    /// Forget a session's waiting frames.
    pub(super) fn remove(&mut self, id: u64) {
        self.lanes.remove(&id);
    }

    fn wait_for_room(&mut self, id: u64, dest: &D) {
        let room = dest.room();
        self.waiting.push(Box::pin(async move { (id, room.await) }));
    }

    fn flush(&mut self, id: u64, alive: bool) {
        let Some(lane) = self.lanes.get_mut(&id) else {
            return;
        };
        while alive && let Some((item, control)) = lane.backlog.pop_front() {
            if control {
                lane.dest.control(item);
                continue;
            }
            match lane.dest.offer(item) {
                Offer::Sent => lane.since = Instant::now(),
                Offer::Full(item) => {
                    lane.backlog.push_front((item, false));
                    let dest = lane.dest.clone();
                    self.wait_for_room(id, &dest);
                    return;
                }
                Offer::Gone => break,
            }
        }
        self.lanes.remove(&id);
    }

    /// Moves waiting frames as queues drain; `None` after some moved, so the
    /// caller can read its connection again if that unblocked it. Resolves with a
    /// session only when it made no room for the whole timeout.
    pub(super) async fn next(&mut self) -> Option<Stalled<D>> {
        let Some(oldest) = self.lanes.values().map(|lane| lane.since).min() else {
            return std::future::pending().await;
        };
        let wake = tokio::select! {
            Some((id, alive)) = self.waiting.next() => Wake::Room(id, alive),
            _ = tokio::time::sleep_until(oldest + self.timeout) => Wake::Deadline,
        };
        match wake {
            Wake::Room(id, alive) => {
                self.flush(id, alive);
                None
            }
            Wake::Deadline => {
                let now = Instant::now();
                let timeout = self.timeout;
                let id = self
                    .lanes
                    .iter()
                    .find(|(_, lane)| lane.since + timeout <= now)
                    .map(|(id, _)| *id)?;
                let lane = self.lanes.remove(&id)?;
                Some(Stalled {
                    id,
                    dest: lane.dest,
                })
            }
        }
    }
}
