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
    target: T,
}

#[derive(Clone, Debug)]
pub struct Service<T, M> {
    target: T,
    make: M,
}

impl<T: Clone> Layer<T> {
    pub fn new(&self, target: T) -> Self {
        Self { target }
    }
}

impl<T: Clone, M> tower::layer::Layer<M> for Layer<T> {
    type Service = Service<T, M>;

    fn layer(&self, make: M) -> Self::Service {
        let target = self.target.clone();
        Service { make, target }
    }
}

impl<Req, T, M, S> tower::Service<Req> for Service<T, M>
where
    T: Target<Req>,
    M: Make<T::Target, Service = S>,
    S: tower::Service<Req> + Clone,
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
