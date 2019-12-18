#![deny(warnings, rust_2018_idioms)]

mod cache;
pub mod error;
pub mod layer;
mod purge;

use self::cache::Cache;
pub use self::layer::Layer;
pub use self::purge::Purge;
use futures::{Async, Future, Poll};
use linkerd2_stack::Make;
use std::hash::Hash;
use std::time::Duration;
use tokio::sync::lock::Lock;
use tracing::debug;

#[derive(Clone)]
pub struct Router<T, M>
where
    T: Clone + Eq + Hash,
    M: Make<T>,
{
    inner: Inner<T, M>,
    _hangup: purge::Handle,
}

pub struct ResponseFuture<T, M>
where
    T: Clone + Eq + Hash,
    M: Make<T>,
{
    target: T,
    make: M,
    cache: Lock<Cache<T, M::Service>>,
}

struct Inner<T, M>
where
    T: Clone + Eq + Hash,
    M: Make<T>,
{
    make: M,
    cache: Lock<Cache<T, M::Service>>,
}

impl<T, M> Clone for Inner<T, M>
where
    T: Clone + Eq + Hash,
    M: Make<T> + Clone,
{
    fn clone(&self) -> Self {
        Self {
            make: self.make.clone(),
            cache: self.cache.clone(),
        }
    }
}

// ===== impl Router =====

impl<T, M> Router<T, M>
where
    T: Clone + Eq + Hash,
    M: Make<T> + Clone,
    M::Service: Clone,
{
    pub fn new(make: M, capacity: usize, max_idle_age: Duration) -> (Self, Purge<T, M::Service>) {
        let cache = Lock::new(Cache::new(capacity, max_idle_age));
        let (purge, _hangup) = Purge::new(cache.clone());
        let router = Self {
            _hangup,
            inner: Inner { make, cache },
        };

        (router, purge)
    }
}

impl<T, M> tower::Service<T> for Router<T, M>
where
    T: Clone + Eq + Hash,
    M: Make<T> + Clone,
    M::Service: Clone,
{
    type Response = M::Service;
    type Error = error::NoCapacity;
    type Future = ResponseFuture<T, M>;

    /// Always ready to serve.
    ///
    /// Graceful backpressure is **not** supported at this level, since each request may
    /// be routed to different resources. Instead, requests should be issued and each
    /// route should support a queue of requests.
    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        Ok(().into())
    }

    /// Routes the request through an underlying service.
    ///
    /// The response fails when the request cannot be routed.
    fn call(&mut self, target: T) -> Self::Future {
        ResponseFuture::new(target, self.inner.make.clone(), self.inner.cache.clone())
    }
}

// ===== impl ResponseFuture =====

impl<T, M> ResponseFuture<T, M>
where
    T: Clone + Eq + Hash,
    M: Make<T>,
    M::Service: Clone,
{
    fn new(target: T, make: M, cache: Lock<Cache<T, M::Service>>) -> Self {
        ResponseFuture {
            target,
            make,
            cache,
        }
    }
}

impl<T, M> Future for ResponseFuture<T, M>
where
    T: Clone + Eq + Hash,
    M: Make<T> + Clone,
    M::Service: Clone,
{
    type Item = M::Service;
    type Error = error::NoCapacity;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        // Aquire the lock for the router cache
        let mut cache = match self.cache.poll_lock() {
            Async::NotReady => return Ok(Async::NotReady),
            Async::Ready(aquired) => aquired,
        };

        // If the target is already cached, route the request to
        // the service; otherwise, try to insert it
        if let Some(service) = cache.access(&self.target) {
            return Ok(service.clone().into());
        }

        // Ensure that there is capacity for a new slot
        if !cache.can_insert() {
            debug!("not enough capacity to insert target into cache");
            return Err(error::NoCapacity(cache.capacity()).into());
        }

        // Make a new service for the target
        debug!("inserting new target into cache");
        let service = self.make.make(self.target.clone());
        cache.insert(self.target.clone(), service.clone());
        Ok(service.into())
    }
}

#[cfg(test)]
mod test_util {
    use super::Make;
    use futures::{future, Async, Poll};
    use std::cell::Cell;
    use std::fmt;
    use std::rc::Rc;
    use tower::Service;

    #[derive(Clone)]
    pub struct Recognize;

    #[derive(Clone, Debug)]
    pub struct MultiplyAndAssign(Rc<Cell<usize>>, bool);

    #[derive(Debug, PartialEq)]
    pub enum MulError {
        AtMax,
        Overflow,
    }

    #[derive(Debug)]
    pub enum Request {
        NotRecognized,
        Recognized(usize),
    }

    // ===== impl Recognize =====

    impl super::Recognize<Request> for Recognize {
        type Target = usize;

        fn recognize(&self, request: &Request) -> Option<Self::Target> {
            match *request {
                Request::NotRecognized => None,
                Request::Recognized(n) => Some(n),
            }
        }
    }

    impl Make<usize> for Recognize {
        type Service = MultiplyAndAssign;

        fn make(&self, _: usize) -> Self::Service {
            MultiplyAndAssign::default()
        }
    }

    // ===== impl MultiplyAndAssign =====

    impl MultiplyAndAssign {
        pub fn new(n: usize) -> Self {
            MultiplyAndAssign(Rc::new(Cell::new(n)), true)
        }

        pub fn never_ready() -> Self {
            MultiplyAndAssign(Rc::new(Cell::new(0)), false)
        }
    }

    impl Default for MultiplyAndAssign {
        fn default() -> Self {
            MultiplyAndAssign::new(1)
        }
    }

    impl Make<usize> for MultiplyAndAssign {
        type Service = MultiplyAndAssign;

        fn make(&self, _: usize) -> Self::Service {
            // Don't use a clone, so that they don't affect the original Stack...
            MultiplyAndAssign(Rc::new(Cell::new(self.0.get())), self.1)
        }
    }

    impl Service<Request> for MultiplyAndAssign {
        type Response = usize;
        type Error = MulError;
        type Future = future::FutureResult<Self::Response, Self::Error>;

        fn poll_ready(&mut self) -> Poll<(), Self::Error> {
            if !self.1 {
                return Ok(Async::NotReady);
            }

            if self.0.get() < ::std::usize::MAX - 1 {
                Ok(().into())
            } else {
                Err(MulError::AtMax)
            }
        }

        fn call(&mut self, request: Request) -> Self::Future {
            let n = match request {
                Request::NotRecognized => unreachable!(),
                Request::Recognized(n) => n,
            };
            let a = self.0.get();
            match a.checked_mul(n) {
                Some(x) => {
                    self.0.set(x);
                    future::ok(x)
                }
                None => future::err(MulError::Overflow),
            }
        }
    }

    impl From<usize> for Request {
        fn from(n: usize) -> Request {
            Request::Recognized(n)
        }
    }

    impl fmt::Display for MulError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "{:?}", self)
        }
    }

    impl std::error::Error for MulError {}
}

#[cfg(test)]
mod tests {
    use super::Make;
    use super::{error, Router};
    use crate::test_util::*;
    use futures::Future;
    use std::time::Duration;
    use std::usize;
    use tower::Service;

    impl<Mk> Router<Request, Recognize, M>
    where
        M: Make<usize> + Clone,
        M::Service: tower::Service<Request, Response = usize> + Clone,
        <M::Service as tower::Service<Request>>::Error: Into<error::Error>,
    {
        fn call_ok(&mut self, request: impl Into<Request>) -> usize {
            let request = request.into();
            let msg = format!("router.call({:?}) should succeed", request);
            self.call(request).wait().expect(&msg)
        }

        fn call_err(&mut self, request: impl Into<Request>) -> error::Error {
            let request = request.into();
            let msg = format!("router.call({:?}) should error", request);
            self.call(request.into()).wait().expect_err(&msg)
        }
    }

    #[test]
    fn invalid() {
        let (mut router, _cache_bg) = Router::new(Recognize, Recognize, 1, Duration::from_secs(60));

        let rsp = router.call_err(Request::NotRecognized);
        assert!(rsp.is::<error::NotRecognized>());
    }

    #[test]
    fn cache_limited_by_capacity() {
        use futures::future;
        use tokio::runtime::current_thread;

        current_thread::run(future::lazy(|| {
            let (mut router, _cache_bg) =
                Router::new(Recognize, Recognize, 1, Duration::from_secs(1));

            let rsp = router.call_ok(2);
            assert_eq!(rsp, 2);

            let rsp = router.call_err(3);
            assert_eq!(
                rsp.downcast_ref::<error::NoCapacity>()
                    .expect("error should be NoCapacity")
                    .0,
                1
            );

            Ok(())
        }))
    }

    #[test]
    fn services_cached() {
        let (mut router, _cache_bg) = Router::new(Recognize, Recognize, 1, Duration::from_secs(60));

        let rsp = router.call_ok(2);
        assert_eq!(rsp, 2);

        let rsp = router.call_ok(2);
        assert_eq!(rsp, 4);
    }

    #[test]
    fn poll_ready_is_called_first() {
        let (mut router, _cache_bg) = Router::new(
            Recognize,
            MultiplyAndAssign::new(usize::MAX),
            1,
            Duration::from_secs(60),
        );

        let err = router.call_err(2);
        assert_eq!(
            err.downcast_ref::<MulError>().expect("should be MulError"),
            &MulError::AtMax,
        );
    }

    #[test]
    fn load_shed_from_inner_services() {
        use tower_load_shed::error::Overloaded;

        let (mut router, _cache_bg) = Router::new(
            Recognize,
            MultiplyAndAssign::never_ready(),
            1,
            Duration::from_secs(1),
        );

        let err = router.call_err(2);
        assert!(err.downcast_ref::<Overloaded>().is_some(), "Not overloaded",);
    }
}
