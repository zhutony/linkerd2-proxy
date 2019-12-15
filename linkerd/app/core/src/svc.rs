// Possibly unused, but useful during development.
#![allow(dead_code)]

use crate::proxy::{buffer, http, pending};
use crate::Error;
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

pub fn proxies() -> Proxies<()> {
    Proxies(())
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

    pub fn boxed<T, A, B>(self) -> Stack<http::boxed::Make<S, A, B>>
    where
        A: 'static,
        S: tower::MakeService<T, http::Request<A>, Response = http::Response<B>>,
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

impl<P> Proxies<P> {
    pub fn push<L: Layer<P>>(self, layer: L) -> Proxies<L::Service> {
        Proxies(layer.layer(self.0))
    }

    /// Validates that this stack makes T-typed targets.
    pub fn makes<T>(self) -> Self
    where
        P: Make<T>,
    {
        self
    }

    /// Validates that this stack makes T-typed targets.
    pub fn makes_clone<T>(self) -> Self
    where
        P: Make<T>,
        P::Service: Clone,
    {
        self
    }

    pub fn into_inner(self) -> P {
        self.0
    }
}

impl<T, P> Make<T> for Proxies<P>
where
    P: Make<T>,
{
    type Service = P::Service;

    fn make(&self, t: T) -> Self::Service {
        self.0.make(t)
    }
}
