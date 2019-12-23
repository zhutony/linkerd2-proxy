// Possibly unused, but useful during development.
#![allow(dead_code)]

use crate::proxy::{buffer, http, pending};
use crate::{cache, Error};
pub use linkerd2_lock as lock;
pub use linkerd2_stack::{self as stack, layer, map_target, Layer, LayerExt, Make, Shared};
pub use linkerd2_timeout::stack as timeout;
use std::time::Duration;
use tower::layer::util::{Identity, Stack as Pair};
use tower::limit::concurrency::ConcurrencyLimitLayer;
use tower::load_shed::LoadShedLayer;
use tower::timeout::TimeoutLayer;
pub use tower::util::{Either, Oneshot};
pub use tower::{service_fn as mk, MakeConnection, MakeService, Service, ServiceExt};
use tower_spawn_ready::SpawnReadyLayer;

#[derive(Clone, Debug)]
pub struct Layers<L>(L);

#[derive(Clone, Debug)]
pub struct Stack<S>(S);

#[derive(Clone, Debug)]
pub struct Proxies<P>(P);

pub fn layers() -> Layers<Identity> {
    Layers(Identity::new())
}

pub fn stack<S>(inner: S) -> Stack<S> {
    Stack(inner)
}

impl<L> Layers<L> {
    pub fn push<O>(self, outer: O) -> Layers<Pair<L, O>> {
        Layers(Pair::new(self.0, outer))
    }

    /// Buffer requests when when the next layer is out of capacity.
    pub fn push_pending(self) -> Layers<Pair<L, pending::Layer>> {
        self.push(pending::layer())
    }

    /// Buffer requests when when the next layer is out of capacity.
    pub fn push_buffer<D, Req>(self, bound: usize, d: D) -> Layers<Pair<L, buffer::Layer<D, Req>>>
    where
        D: buffer::Deadline<Req>,
        Req: Send + 'static,
    {
        self.push(buffer::layer(bound, d))
    }

    pub fn push_spawn_ready(self) -> Layers<Pair<L, SpawnReadyLayer>> {
        self.push(SpawnReadyLayer::new())
    }

    pub fn push_lock(self) -> Layers<Pair<L, lock::Layer>> {
        self.push(lock::Layer)
    }

    pub fn boxed<A, B>(self) -> Layers<Pair<L, http::boxed::Layer<A, B>>>
    where
        A: 'static,
        B: hyper::body::Payload<Data = http::boxed::Data, Error = Error> + 'static,
    {
        self.push(http::boxed::Layer::new())
    }
}

impl<M, L: Layer<M>> Layer<M> for Layers<L> {
    type Service = L::Service;

    fn layer(&self, inner: M) -> Self::Service {
        self.0.layer(inner)
    }
}

impl<S> Stack<S> {
    pub fn push<L: Layer<S>>(self, layer: L) -> Stack<L::Service> {
        Stack(layer.layer(self.0))
    }

    /// Buffer requests when when the next layer is out of capacity.
    pub fn push_pending(self) -> Stack<pending::MakePending<S>> {
        self.push(pending::layer())
    }

    pub fn push_lock(self) -> Stack<lock::Lock<S>> {
        self.push(lock::Layer)
    }

    /// Buffer requests when when the next layer is out of capacity.
    pub fn push_buffer<D, Req>(self, bound: usize, d: D) -> Stack<buffer::Make<S, D, Req>>
    where
        D: buffer::Deadline<Req>,
        Req: Send + 'static,
    {
        self.push(buffer::layer(bound, d))
    }

    pub fn push_spawn_ready(self) -> Stack<tower_spawn_ready::MakeSpawnReady<S>> {
        self.push(SpawnReadyLayer::new())
    }

    pub fn push_concurrency_limit(self, max: usize) -> Stack<tower::limit::ConcurrencyLimit<S>> {
        self.push(ConcurrencyLimitLayer::new(max))
    }

    pub fn push_load_shed(self) -> Stack<tower::load_shed::LoadShed<S>> {
        self.push(LoadShedLayer::new())
    }

    pub fn push_timeout(self, timeout: Duration) -> Stack<tower::timeout::Timeout<S>> {
        self.push(TimeoutLayer::new(timeout))
    }

    pub fn push_wrap<L: Clone>(self, layer: L) -> Stack<wrap::Wrap<L, S>> {
        self.push(wrap::Layer(layer))
    }

    pub fn spawn_cache<T>(
        self,
        capacity: usize,
        max_idle_age: Duration,
    ) -> Stack<cache::Service<T, S>>
    where
        T: Clone + Eq + std::hash::Hash + Send + 'static,
        S: Make<T> + Send + 'static,
        S::Service: Clone + Send + 'static,
    {
        Stack(
            cache::Layer::new(capacity, max_idle_age)
                .layer(self.0)
                .spawn(),
        )
    }

    pub fn boxed<A, B>(self) -> Stack<http::boxed::BoxedService<A>>
    where
        A: 'static,
        S: tower::Service<http::Request<A>, Response = http::Response<B>> + Send + 'static,
        S::Future: Send + 'static,
        S::Error: Into<Error> + 'static,
        B: hyper::body::Payload<Data = http::boxed::Data, Error = Error> + 'static,
    {
        self.push(http::boxed::Layer::new())
    }

    /// Validates that this stack serves T-typed targets.
    pub fn makes<T>(self) -> Self
    where
        S: Make<T>,
    {
        self
    }

    pub fn makes_clone<T>(self) -> Self
    where
        S: Make<T>,
        S::Service: Clone,
    {
        self
    }

    /// Validates that this stack serves T-typed targets.
    pub fn serves<T>(self) -> Self
    where
        S: Service<T>,
    {
        self
    }

    /// Validates that this stack serves T-typed targets.
    pub fn routes<T, Req>(self) -> Self
    where
        S: Make<T>,
        S::Service: Service<Req>,
    {
        self
    }

    pub fn into_inner(self) -> S {
        self.0
    }
}

impl<T, S> Make<T> for Stack<S>
where
    S: Make<T>,
{
    type Service = S::Service;

    fn make(&self, t: T) -> Self::Service {
        self.0.make(t)
    }
}

impl<T, S> Service<T> for Stack<S>
where
    S: Service<T>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self) -> futures::Poll<(), Self::Error> {
        self.0.poll_ready()
    }

    fn call(&mut self, t: T) -> Self::Future {
        self.0.call(t)
    }
}

pub mod wrap {
    use super::{Make, Service};
    use futures::{try_ready, Future, Poll};

    #[derive(Clone, Debug)]
    pub struct Layer<L>(pub(super) L);

    #[derive(Clone, Debug)]
    pub struct Wrap<L, M> {
        layer: L,
        make: M,
    }

    impl<L: Clone, M> tower::layer::Layer<M> for Layer<L> {
        type Service = Wrap<L, M>;

        fn layer(&self, make: M) -> Self::Service {
            Self::Service {
                layer: self.0.clone(),
                make,
            }
        }
    }

    impl<T, L, M> Make<T> for Wrap<L, M>
    where
        M: Make<T>,
        L: tower::layer::Layer<M::Service>,
    {
        type Service = L::Service;

        fn make(&self, target: T) -> Self::Service {
            self.layer.layer(self.make.make(target))
        }
    }

    impl<T, L, M> Service<T> for Wrap<L, M>
    where
        M: Service<T>,
        L: tower::layer::Layer<M::Response> + Clone,
    {
        type Response = L::Service;
        type Error = M::Error;
        type Future = Wrap<L, M::Future>;

        fn poll_ready(&mut self) -> Poll<(), Self::Error> {
            self.make.poll_ready()
        }

        fn call(&mut self, target: T) -> Self::Future {
            Self::Future {
                layer: self.layer.clone(),
                make: self.make.call(target),
            }
        }
    }

    impl<L, F> Future for Wrap<L, F>
    where
        F: Future,
        L: tower::layer::Layer<F::Item>,
    {
        type Item = L::Service;
        type Error = F::Error;

        fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
            let svc = try_ready!(self.make.poll());
            Ok(self.layer.layer(svc).into())
        }
    }
}
