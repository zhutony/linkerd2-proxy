use futures::Poll;
use linkerd2_stack::Make;
use std::hash::Hash;
use tower::util::{Oneshot, ServiceExt};

pub trait Key<T> {
    type Key: Clone + Eq + Hash;

    fn key(&self, t: &T) -> Self::Key;
}

#[derive(Clone, Debug)]
pub struct Layer<T> {
    make_key: T,
}

#[derive(Clone, Debug)]
pub struct MakeRouter<T, M> {
    make_key: T,
    make_route: M,
}

#[derive(Clone, Debug)]
pub struct Router<T, M> {
    key: T,
    make: M,
}

impl<K: Clone> Layer<K> {
    pub fn new(make_key: K) -> Self {
        Self { make_key }
    }
}

impl<K: Clone, M> tower::layer::Layer<M> for Layer<K> {
    type Service = MakeRouter<K, M>;

    fn layer(&self, make_route: M) -> Self::Service {
        MakeRouter {
            make_route,
            make_key: self.make_key.clone(),
        }
    }
}

impl<T, K, M> Make<T> for MakeRouter<K, M>
where
    K: Make<T>,
    M: Clone,
{
    type Service = Router<K::Service, M>;

    fn make(&self, t: T) -> Self::Service {
        Router {
            key: self.make_key.make(t),
            make: self.make_route.clone(),
        }
    }
}

impl<U, S, K, M> tower::Service<U> for Router<K, M>
where
    K: Key<U>,
    M: Make<K::Key, Service = S>,
    S: tower::Service<U>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = Oneshot<S, U>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        Ok(().into())
    }

    fn call(&mut self, req: U) -> Self::Future {
        let key = self.key.key(&req);
        self.make.make(key).oneshot(req)
    }
}

impl<T, K, F> Key<T> for F
where
    F: Fn(&T) -> K,
    K: Clone + Eq + Hash,
{
    type Key = K;

    fn key(&self, t: &T) -> Self::Key {
        (self)(t)
    }
}
