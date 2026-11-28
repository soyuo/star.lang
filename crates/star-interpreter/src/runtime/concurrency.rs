//! Local async task, channel, and mutex primitives for the reference runtime.
//!
//! These primitives run on a Tokio current-thread scheduler. Guest values are
//! intentionally `!Send`; CPU-parallel work belongs in the isolated worker VM.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::fmt;
use std::future::Future;
use std::rc::Rc;
use std::time::Duration;

use tokio::runtime::{Builder, Runtime};
use tokio::sync::{oneshot, Notify};
use tokio::task::{JoinError, JoinHandle, LocalSet};

/// A current-thread Tokio runtime for tasks that may own non-`Send` Star values.
pub struct TaskRuntime {
    runtime: Runtime,
}

impl TaskRuntime {
    /// Creates a current-thread runtime with timer support.
    pub fn new() -> std::io::Result<Self> {
        Ok(Self {
            runtime: Builder::new_current_thread().enable_time().build()?,
        })
    }

    /// Drives the local task set until `future` completes.
    pub fn block_on<F: Future>(&self, future: F) -> F::Output {
        let local = LocalSet::new();
        self.runtime.block_on(local.run_until(future))
    }

    /// Spawns a non-`Send` future in the currently entered local task set.
    ///
    /// Call this from within [`TaskRuntime::block_on`].
    pub fn spawn_local<F>(&self, id: u64, future: F) -> Task<F::Output>
    where
        F: Future + 'static,
        F::Output: 'static,
    {
        Task {
            id,
            join: tokio::task::spawn_local(future),
        }
    }

    /// Returns execution to another ready local task.
    pub async fn yield_now() {
        tokio::task::yield_now().await;
    }

    /// Suspends the current task for the requested duration.
    pub async fn sleep(duration: Duration) {
        tokio::time::sleep(duration).await;
    }
}

/// A cancellable handle to a local async task.
pub struct Task<T> {
    id: u64,
    join: JoinHandle<T>,
}

impl<T> Task<T> {
    /// Returns the stable runtime task identifier.
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Requests cancellation. The task may still be in the process of stopping.
    pub fn cancel(&self) {
        self.join.abort();
    }

    /// Waits for task completion, returning cancellation and panic status.
    pub async fn join(self) -> Result<T, JoinError> {
        self.join.await
    }
}

/// A cloneable asynchronous channel with bounded and rendezvous modes.
#[derive(Clone)]
pub struct Channel<T> {
    inner: Rc<ChannelInner<T>>,
}

struct ChannelInner<T> {
    capacity: usize,
    state: RefCell<ChannelState<T>>,
    changed: Notify,
}

struct ChannelState<T> {
    queue: VecDeque<T>,
    senders: VecDeque<PendingSend<T>>,
    receivers: VecDeque<oneshot::Sender<T>>,
    closed: bool,
}

struct PendingSend<T> {
    value: T,
    accepted: oneshot::Sender<()>,
}

/// A channel operation failed because its peer closed or the channel was closed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ChannelClosed;

impl fmt::Display for ChannelClosed {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("channel is closed")
    }
}

impl std::error::Error for ChannelClosed {}

impl<T> Channel<T> {
    /// Creates a channel. Capacity zero is a rendezvous channel: each send waits
    /// until a receiver accepts that exact value.
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: Rc::new(ChannelInner {
                capacity,
                state: RefCell::new(ChannelState {
                    queue: VecDeque::new(),
                    senders: VecDeque::new(),
                    receivers: VecDeque::new(),
                    closed: false,
                }),
                changed: Notify::new(),
            }),
        }
    }

    /// Sends a value, waiting while a bounded channel is full.
    pub async fn send(&self, value: T) -> Result<(), ChannelClosed> {
        let mut value = Some(value);
        loop {
            let notified = self.inner.changed.notified();
            enum SendAction {
                Accepted,
                Wait(oneshot::Receiver<()>),
                Retry,
            }
            let action = {
                let mut state = self.inner.state.borrow_mut();
                if state.closed {
                    return Err(ChannelClosed);
                }
                if let Some(receiver) = state.receivers.pop_front() {
                    if let Some(pending_value) = value.take() {
                        match receiver.send(pending_value) {
                            Ok(()) => SendAction::Accepted,
                            Err(pending_value) => {
                                value = Some(pending_value);
                                self.inner.changed.notify_one();
                                SendAction::Retry
                            }
                        }
                    } else {
                        SendAction::Retry
                    }
                } else if self.inner.capacity > 0 && state.queue.len() < self.inner.capacity {
                    if let Some(value) = value.take() {
                        state.queue.push_back(value);
                    }
                    SendAction::Accepted
                } else if self.inner.capacity == 0 {
                    let (accepted, wait) = oneshot::channel();
                    state.senders.push_back(PendingSend {
                        value: value.take().expect("value is submitted once"),
                        accepted,
                    });
                    SendAction::Wait(wait)
                } else {
                    SendAction::Retry
                }
            };
            match action {
                SendAction::Accepted => {
                    self.inner.changed.notify_one();
                    return Ok(());
                }
                SendAction::Wait(wait) => {
                    self.inner.changed.notify_one();
                    wait.await.map_err(|_| ChannelClosed)?;
                    return Ok(());
                }
                SendAction::Retry => notified.await,
            }
        }
    }

    /// Receives the next value, waiting while the channel is empty.
    pub async fn recv(&self) -> Result<T, ChannelClosed> {
        enum ReceiveAction<T> {
            Value(T),
            Sender(T, oneshot::Sender<()>),
            Wait(oneshot::Receiver<T>),
        }
        let action = {
            let mut state = self.inner.state.borrow_mut();
            if let Some(value) = state.queue.pop_front() {
                ReceiveAction::Value(value)
            } else if let Some(sender) = state.senders.pop_front() {
                ReceiveAction::Sender(sender.value, sender.accepted)
            } else if state.closed {
                return Err(ChannelClosed);
            } else {
                let (sender, receiver) = oneshot::channel();
                state.receivers.push_back(sender);
                ReceiveAction::Wait(receiver)
            }
        };
        match action {
            ReceiveAction::Value(value) => {
                self.inner.changed.notify_one();
                Ok(value)
            }
            ReceiveAction::Sender(value, accepted) => {
                let _ = accepted.send(());
                self.inner.changed.notify_one();
                Ok(value)
            }
            ReceiveAction::Wait(receiver) => {
                self.inner.changed.notify_one();
                let value = receiver.await.map_err(|_| ChannelClosed)?;
                self.inner.changed.notify_one();
                Ok(value)
            }
        }
    }

    /// Closes the channel and wakes pending operations.
    pub fn close(&self) {
        let mut state = self.inner.state.borrow_mut();
        state.closed = true;
        state.senders.clear();
        state.receivers.clear();
        drop(state);
        self.inner.changed.notify_waiters();
    }
}

/// Result of an owner-checked asynchronous mutex operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MutexError {
    /// The current task already owns the non-reentrant mutex.
    AlreadyOwned,
    /// The current task is not the owner.
    NotOwner,
}

impl fmt::Display for MutexError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyOwned => formatter.write_str("mutex is not reentrant"),
            Self::NotOwner => formatter.write_str("current task does not own the mutex"),
        }
    }
}

impl std::error::Error for MutexError {}

/// An asynchronous non-reentrant mutex whose unlock is checked against task ID.
#[derive(Clone, Default)]
pub struct TaskMutex {
    inner: Rc<MutexInner>,
}

#[derive(Default)]
struct MutexInner {
    owner: RefCell<Option<u64>>,
    changed: Notify,
}

impl TaskMutex {
    /// Waits until the mutex is free and assigns ownership to `task_id`.
    pub async fn lock(&self, task_id: u64) -> Result<(), MutexError> {
        loop {
            let notified = self.inner.changed.notified();
            {
                let mut owner = self.inner.owner.borrow_mut();
                match *owner {
                    None => {
                        *owner = Some(task_id);
                        return Ok(());
                    }
                    Some(owner) if owner == task_id => return Err(MutexError::AlreadyOwned),
                    Some(_) => {}
                }
            }
            notified.await;
        }
    }

    /// Releases the mutex only when called by its current owner.
    pub fn unlock(&self, task_id: u64) -> Result<(), MutexError> {
        let mut owner = self.inner.owner.borrow_mut();
        if *owner != Some(task_id) {
            return Err(MutexError::NotOwner);
        }
        *owner = None;
        drop(owner);
        self.inner.changed.notify_one();
        Ok(())
    }
}
