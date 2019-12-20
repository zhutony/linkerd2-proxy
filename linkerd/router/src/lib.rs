use futures::Poll;
use linkerd2_stack::Make;
use std::hash::Hash;
use tower::util::{Oneshot, ServiceExt};

pub trait Target<Req> {
    type Target: Clone + Eq + Hash;

    fn target(&self, req: &Req) -> Self::Target;
}

#[derive(Clone, Debug)]
pub struct Layer<T> {
    make_target: T,
}

#[derive(Clone, Debug)]
pub struct MakeRouter<T, M> {
    make_target: T,
    make_route: M,
}

#[derive(Clone, Debug)]
pub struct Router<T, M> {
    target: T,
    make: M,
}

impl<T: Clone> Layer<T> {
    pub fn new(make_target: T) -> Self {
        Self { make_target }
    }
}

impl<T: Clone, M> tower::layer::Layer<M> for Layer<T> {
    type Service = MakeRouter<T, M>;

    fn layer(&self, make_route: M) -> Self::Service {
        MakeRouter {
            make_route,
            make_target: self.make_target.clone(),
        }
    }
}

impl<K, T: Make<K>, M: Clone> Make<K> for MakeRouter<T, M> {
    type Service = Router<T::Service, M>;

    fn make(&self, key: K) -> Self::Service {
        Router {
            make: self.make_route.clone(),
            target: self.make_target.make(key),
        }
    }
}

impl<Req, S, T, M> tower::Service<Req> for Router<T, M>
where
    T: Target<Req>,
    M: Make<T::Target, Service = S>,
    S: tower::Service<Req>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = Oneshot<S, Req>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        Ok(().into())
    }

    fn call(&mut self, req: Req) -> Self::Future {
        let target = self.target.target(&req);
        self.make.make(target).oneshot(req)
    }
}

impl<Req, T, F> Target<Req> for F
where
    F: Fn(&Req) -> T,
    T: Clone + Eq + Hash,
{
    type Target = T;

    fn target(&self, req: &Req) -> Self::Target {
        (self)(req)
    }
}
