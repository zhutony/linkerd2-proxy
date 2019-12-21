#![deny(warnings, rust_2018_idioms)]

use futures::{try_ready, Poll};
use tokio::sync::lock;

#[derive(Copy, Clone)]
pub struct Layer;

#[derive(Debug)]
pub struct Lock<S> {
    lock: lock::Lock<S>,
    locked: Option<lock::LockGuard<S>>,
}

impl<M> tower::layer::Layer<M> for Layer {
    type Service = Lock<M>;

    fn layer(&self, inner: M) -> Self::Service {
        Self::Service {
            lock: lock::Lock::new(inner),
            locked: None,
        }
    }
}

impl<S> Clone for Lock<S> {
    fn clone(&self) -> Self {
        Self {
            lock: self.lock.clone(),
            locked: None,
        }
    }
}

impl<T, S: tower::Service<T>> tower::Service<T> for Lock<S> {
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        loop {
            if let Some(inner) = self.locked.as_mut() {
                return inner.poll_ready();
            }

            let locked = try_ready!(Ok(self.lock.poll_lock()));
            self.locked = Some(locked);
        }
    }

    fn call(&mut self, t: T) -> Self::Future {
        self.locked.take().expect("called before ready").call(t)
    }
}
