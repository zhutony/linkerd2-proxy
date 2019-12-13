use super::LostDaemon;
use futures::{task, Async, Future, Poll};
use indexmap::IndexMap;
use std::hash::Hash;
use std::sync::{Arc, Mutex};
use tokio::sync::oneshot;

pub fn channel<Req, F>(capacity: usize) -> (Push<Req, F>, Pop<Req, F>) {
    let inner = Arc::new(Mutex::new(Inner {
        next_token: 0,
        in_flight: IndexMap::default(),
        push_tasks: Vec::new(),
        pop_task: None,
    }));
    let push = Push {
        inner: inner.clone(),
        capacity,
    };
    let pop = Pop { inner };
    (push, pop)
}

pub struct Push<Req, F> {
    inner: Arc<Mutex<Inner<Req, F>>>,
    capacity: usize,
}

pub struct Pop<Req, F> {
    inner: Arc<Mutex<Inner<Req, F>>>,
}

struct Inner<Req, F> {
    next_token: usize,
    in_flight: IndexMap<Token, InFlight<Req, F>>,
    push_tasks: Vec<task::Task>,
    pop_task: Option<task::Task>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct Token(usize);

pub struct InFlight<Req, F> {
    pub request: Req,
    pub tx: oneshot::Sender<F>,
}

pub enum PushFuture<Req, F> {
    Poisoned,
    Pending {
        token: Token,
        rx: oneshot::Receiver<F>,
        inner: Arc<Mutex<Inner<Req, F>>>,
    },
}

impl<Req, F> Push<Req, F> {
    fn poll_ready(&mut self) -> Poll<(), LostDaemon> {
        let mut inner = match self.inner.lock() {
            Err(_) => return Err(LostDaemon(())),
            Ok(inner) => inner,
        };
        if inner.in_flight.len() < self.capacity {
            return Ok(Async::Ready(()));
        }
        inner.push_tasks.push(task::current());
        Ok(Async::NotReady)
    }

    fn push(&mut self, request: Req) -> PushFuture<Req, F> {
        let mut inner = match self.inner.lock() {
            Err(_) => return PushFuture::Poisoned,
            Ok(inner) => inner,
        };

        let token = Token(inner.next_token);
        let (tx, rx) = oneshot::channel();
        inner.in_flight.insert(token, InFlight { request, tx });

        inner.next_token = inner.next_token.wrapping_add(1);
        if let Some(task) = inner.pop_task.take() {
            task.notify();
        }

        PushFuture::Pending {
            token,
            rx,
            inner: self.inner.clone(),
        }
    }
}

impl<Req, F> Clone for Push<Req, F> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            capacity: self.capacity,
        }
    }
}

impl<Req, F> Pop<Req, F> {
    fn poll_pop(&mut self, request: Req) -> Poll<InFlight<Req, F>, LostDaemon> {
        let mut inner = match self.inner.lock() {
            Err(_) => return Err(LostDaemon(())),
            Ok(inner) => inner,
        };

        if let Some((_, node)) = inner.in_flight.pop() {
            return Ok(Async::Ready(node));
        }

        inner.pop_task = Some(task::current());
        Ok(Async::NotReady)
    }

    pub fn poll_complete(&mut self) -> Poll<(), LostDaemon> {
        if Arc::strong_count(&self.inner) == 1 {
            return Ok(Async::Ready(()));
        }

        let mut inner = match self.inner.lock() {
            Err(_) => return Err(LostDaemon(())),
            Ok(inner) => inner,
        };

        inner.pop_task = Some(task::current());
        Ok(Async::NotReady)
    }
}

impl<Req, F> Future for PushFuture<Req, F> {
    type Item = F;
    type Error = LostDaemon;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        match self {
            PushFuture::Poisoned => Err(LostDaemon(())),
            PushFuture::Pending { rx, .. } => rx.poll().map_err(|_| LostDaemon(())),
        }
    }
}

impl<Req, F> Drop for PushFuture<Req, F> {
    fn drop(&mut self) {
        if let PushFuture::Pending { token, inner, .. } = self {
            let ref_count = Arc::strong_count(&inner);
            if let Ok(inner) = inner.lock() {
                debug_assert!(ref_count >= inner.in_flight.len());

                inner.in_flight.swap_remove(token);

                // Notify the pop task if
                //
                // A ref_count of 2 indicates that only this future and the poll
                // task hold a reference to `inner`.
                if ref_count == 2 {
                    debug_assert!(inner.in_flight.is_empty());
                    if let Some(task) = inner.pop_task.take() {
                        task.notify();
                    }
                    return;
                }

                // Notify push tasks that capacity is available
                while let Some(task) = inner.push_tasks.pop() {
                    task.notify();
                }
            }
        }
    }
}
