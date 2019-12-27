// Possibly unused, but useful during development.
#![allow(dead_code)]

use crate::proxy::{buffer, http, pending, ready};
use crate::{cache, Error};
use linkerd2_concurrency_limit as concurrency_limit;
pub use linkerd2_lock as lock;
pub use linkerd2_stack::{
    self as stack, layer, map_target, per_make, Layer, LayerExt, Make, Shared,
};
pub use linkerd2_timeout as timeout;
use std::time::Duration;
use tower::layer::util::{Identity, Stack as Pair};
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
    pub fn push_buffer<Req>(self, bound: usize) -> Layers<Pair<L, buffer::Layer<Req>>>
    where
        Req: Send + 'static,
    {
        self.push(buffer::Layer::new(bound))
    }

    pub fn push_spawn_ready(self) -> Layers<Pair<L, SpawnReadyLayer>> {
        self.push(SpawnReadyLayer::new())
    }

    pub fn push_lock(self) -> Layers<Pair<L, lock::Layer>> {
        self.push(lock::Layer::new())
    }

    pub fn push_concurrency_limit(self, max: usize) -> Layers<Pair<L, concurrency_limit::Layer>> {
        self.push(concurrency_limit::Layer::new(max))
    }

    pub fn push_load_shed(self) -> Layers<Pair<L, load_shed::Layer>> {
        self.push(load_shed::Layer)
    }

    pub fn push_ready_timeout(self, timeout: Duration) -> Layers<Pair<L, timeout::ready::Layer>> {
        self.push(timeout::ready::Layer::new(timeout))
    }

    pub fn boxed<A, B>(self) -> Layers<Pair<L, http::boxed::Layer<A, B>>>
    where
        A: 'static,
        B: hyper::body::Payload<Data = http::boxed::Data, Error = Error> + 'static,
    {
        self.push(http::boxed::Layer::new())
    }

    pub fn push_per_make<O: Clone>(self, layer: O) -> Layers<Pair<L, per_make::Layer<O>>> {
        self.push(per_make::layer(layer))
    }

    pub fn per_make(self) -> Layers<per_make::Layer<L>> {
        Layers(per_make::layer(self.0))
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

    pub fn push_pending(self) -> Stack<pending::MakePending<S>> {
        self.push(pending::layer())
    }

    pub fn push_ready<Req>(self) -> Stack<ready::MakeReady<S, Req>> {
        self.push(ready::Layer::new())
    }

    pub fn push_lock(self) -> Stack<lock::Lock<S>> {
        self.push(lock::Layer::new())
    }

    /// Buffer requests when when the next layer is out of capacity.
    pub fn push_buffer<Req>(self, bound: usize) -> Stack<buffer::Buffer<S, Req>>
    where
        Req: Send + 'static,
        S: Service<Req> + Send + 'static,
        S::Error: Into<Error> + Send + Sync,
        S::Future: Send + 'static,
    {
        self.push(buffer::Layer::new(bound))
    }

    pub fn push_spawn_ready(self) -> Stack<tower_spawn_ready::MakeSpawnReady<S>> {
        self.push(SpawnReadyLayer::new())
    }

    pub fn push_concurrency_limit(
        self,
        max: usize,
    ) -> Stack<concurrency_limit::ConcurrencyLimit<S>> {
        self.push(concurrency_limit::Layer::new(max))
    }

    pub fn push_load_shed(self) -> Stack<load_shed::LoadShed<S>> {
        self.push(load_shed::Layer)
    }

    pub fn push_timeout(self, timeout: Duration) -> Stack<tower::timeout::Timeout<S>> {
        self.push(tower::timeout::TimeoutLayer::new(timeout))
    }

    pub fn push_ready_timeout(self, timeout: Duration) -> Stack<timeout::ready::TimeoutReady<S>> {
        self.push(layer::mk(|inner| {
            timeout::ready::TimeoutReady::new(inner, timeout)
        }))
    }

    pub fn push_per_make<L: Clone>(self, layer: L) -> Stack<per_make::PerMake<L, S>> {
        self.push(per_make::layer(layer))
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

/// Proivdes a cloneable Layer, unlike tower::load_shed.
pub mod load_shed {
    pub use tower::load_shed::LoadShed;

    #[derive(Copy, Clone, Debug)]
    pub struct Layer;

    impl<S> super::Layer<S> for Layer {
        type Service = LoadShed<S>;

        fn layer(&self, inner: S) -> Self::Service {
            LoadShed::new(inner)
        }
    }
}
