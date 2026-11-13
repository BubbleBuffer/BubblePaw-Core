//! A connection's outbound frame queue. Data frames respect the capacity and
//! wait for room; control frames (incoming, accept, close) always get in, in
//! order with the data, so none is lost while the connection lives.
use super::lanes::{Dest, Offer};
use std::{
    collections::VecDeque,
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};
use tokio::{sync::Notify, time::Instant};

struct Queue<T> {
    items: VecDeque<(T, bool)>,
    data: usize,
    control: usize,
    closed: bool,
}

struct Shared<T> {
    queue: Mutex<Queue<T>>,
    readable: Notify,
    room: Notify,
    capacity: usize,
}

impl<T> Shared<T> {
    fn lock(&self) -> MutexGuard<'_, Queue<T>> {
        self.queue.lock().unwrap_or_else(|error| error.into_inner())
    }
}

pub(super) struct Outbox<T>(Arc<Shared<T>>);
pub(super) struct Inbox<T>(Arc<Shared<T>>);

impl<T> Clone for Outbox<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

pub(super) fn channel<T>(capacity: usize) -> (Outbox<T>, Inbox<T>) {
    let shared = Arc::new(Shared {
        queue: Mutex::new(Queue {
            items: VecDeque::new(),
            data: 0,
            control: 0,
            closed: false,
        }),
        readable: Notify::new(),
        room: Notify::new(),
        capacity,
    });
    (Outbox(shared.clone()), Inbox(shared))
}

/// What became of a data frame sent with [`Outbox::send_data`].
pub(super) enum Sent {
    Queued,
    /// The receiving connection ended.
    Gone,
    /// No room opened within the wait.
    Stalled,
}

async fn room<T>(shared: Arc<Shared<T>>) -> bool {
    loop {
        let notified = shared.room.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        {
            let queue = shared.lock();
            if queue.closed {
                return false;
            }
            if queue.data < shared.capacity {
                return true;
            }
        }
        notified.await;
    }
}

impl<T> Outbox<T> {
    fn push(&self, item: T, control: bool) -> Offer<T> {
        let mut queue = self.0.lock();
        if queue.closed {
            return Offer::Gone;
        }
        if control {
            queue.control += 1;
        } else if queue.data >= self.0.capacity {
            return Offer::Full(item);
        } else {
            queue.data += 1;
        }
        queue.items.push_back((item, control));
        drop(queue);
        self.0.readable.notify_one();
        Offer::Sent
    }

    /// Control frames queued and not yet taken.
    pub(super) fn control_len(&self) -> usize {
        self.0.lock().control
    }

    /// Queue a control frame ignoring the data capacity. False when the
    /// receiving connection has ended.
    pub(super) fn control(&self, item: T) -> bool {
        !matches!(self.push(item, true), Offer::Gone)
    }

    /// Queue a data frame, waiting up to `wait` for room.
    pub(super) async fn send_data(&self, mut item: T, wait: Duration) -> Sent {
        let deadline = Instant::now() + wait;
        loop {
            match self.push(item, false) {
                Offer::Sent => return Sent::Queued,
                Offer::Gone => return Sent::Gone,
                Offer::Full(back) => item = back,
            }
            match tokio::time::timeout_at(deadline, room(self.0.clone())).await {
                Ok(true) => {}
                Ok(false) => return Sent::Gone,
                Err(_) => return Sent::Stalled,
            }
        }
    }
}

impl<T: Send + 'static> Dest for Outbox<T> {
    type Item = T;
    fn offer(&self, item: T) -> Offer<T> {
        self.push(item, false)
    }
    fn control(&self, item: T) -> bool {
        Outbox::control(self, item)
    }
    fn room(&self) -> Pin<Box<dyn Future<Output = bool> + Send>> {
        Box::pin(room(self.0.clone()))
    }
}

impl<T> Inbox<T> {
    /// The next frame in order.
    pub(super) async fn recv(&mut self) -> T {
        loop {
            let notified = self.0.readable.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let popped = {
                let mut queue = self.0.lock();
                let popped = queue.items.pop_front();
                if let Some((_, control)) = &popped {
                    if *control {
                        queue.control -= 1;
                    } else {
                        queue.data -= 1;
                    }
                }
                popped
            };
            if let Some((item, control)) = popped {
                if !control {
                    self.0.room.notify_waiters();
                }
                return item;
            }
            notified.await;
        }
    }
}

impl<T> Drop for Inbox<T> {
    fn drop(&mut self) {
        self.0.lock().closed = true;
        self.0.room.notify_waiters();
    }
}
