#![deny(warnings, rust_2018_idioms)]

use self::cache::Cache;
pub use self::layer::Layer;
pub use self::purge::Purge;
use futures::{future, try_ready, Async, Poll};
use linkerd2_stack::Make;
use std::hash::Hash;
use std::time::Duration;
use tokio::sync::lock::{Lock, LockGuard};
use tracing::debug;

mod cache;
pub mod error;
pub mod layer;
mod purge;

pub struct Service<T, M>
where
    T: Clone + Eq + Hash,
    M: Make<T>,
{
    make: M,
    cache: Lock<Cache<T, M::Service>>,
    lock: Option<LockGuard<Cache<T, M::Service>>>,
    _hangup: purge::Handle,
}

// === impl Service ===

impl<T, M> Service<T, M>
where
    T: Clone + Eq + Hash,
    M: Make<T>,
    M::Service: Clone,
{
    pub fn new(make: M, capacity: usize, max_idle_age: Duration) -> (Self, Purge<T, M::Service>) {
        let cache = Lock::new(Cache::new(capacity, max_idle_age));
        let (purge, _hangup) = Purge::new(cache.clone());
        let router = Self {
            cache,
            make,
            _hangup,
            lock: None,
        };

        (router, purge)
    }
}

impl<T, M> Clone for Service<T, M>
where
    T: Clone + Eq + Hash,
    M: Make<T> + Clone,
    M::Service: Clone,
{
    fn clone(&self) -> Self {
        Self {
            make: self.make.clone(),
            cache: self.cache.clone(),
            _hangup: self._hangup.clone(),
            lock: None,
        }
    }
}

impl<T, M> tower::Service<T> for Service<T, M>
where
    T: Clone + Eq + Hash,
    M: Make<T>,
    M::Service: Clone,
{
    type Response = M::Service;
    type Error = error::NoCapacity;
    type Future = future::FutureResult<Self::Response, Self::Error>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        let lock = try_ready!(Ok(self.cache.poll_lock()));
        self.lock = Some(lock);
        Ok(Async::Ready(()))
    }

    fn call(&mut self, target: T) -> Self::Future {
        let mut cache = self.lock.take().expect("not ready");

        if let Some(service) = cache.access(&target) {
            return future::ok(service.clone().into());
        }

        if !cache.can_insert() {
            debug!("not enough capacity to insert target into cache");
            return future::err(error::NoCapacity(cache.capacity()).into());
        }

        // Make a new service for the target
        debug!("inserting new target into cache");
        let service = self.make.make(target.clone());
        cache.insert(target.clone(), service.clone());
        future::ok(service.into())
    }
}
