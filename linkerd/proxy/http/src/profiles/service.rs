//! A stack module that produces a Service that routes requests through alternate
//! middleware configurations
//!
//! As the router's Stack is built, a destination is extracted from the stack's
//! target and it is used to get route profiles from ` GetRoutes` implementation.
//!
//! Each route uses a shared underlying concrete dst router.  The concrete dst
//! router picks a concrete dst (NameAddr) from the profile's `dst_overrides` if
//! they exist, or uses the router's target's addr if no `dst_overrides` exist.
//! The concrete dst router uses the concrete dst as the target for the
//! underlying stack.

use super::concrete;
use super::requests::Requests;
use super::{CanGetDestination, GetRoutes, Route, Routes, WithAddr, WithRoute};
use futures::{future, try_ready, Async, Future, Poll, Stream};
use linkerd2_error::{Error, Never};
use linkerd2_stack::{Make, Proxy};
use rand::{rngs::SmallRng, SeedableRng};
use tracing::debug;

#[derive(Clone, Debug)]
pub struct Layer<G, RMake> {
    get_routes: G,
    make_route: RMake,
    traffic_split: TrafficSplit,
    /// This is saved into a field so that the same `Arc`s are used and
    /// cloned, instead of calling `Route::default()` every time.
    default_route: Route,
}

#[derive(Clone, Debug)]
pub struct MakeSvc<G, RMake, CMake> {
    default_route: Route,
    get_routes: G,
    make_route: RMake,
    make_concrete: CMake,
    traffic_split: TrafficSplit,
}

pub struct Service<T, P, RMake, CMake>
where
    T: WithRoute,
    RMake: Make<T::Output>,
    CMake: Make<T>,
{
    profiles: Option<P>,
    requests: Requests<T, RMake>,
    concrete: Concrete<T, CMake>,
    concrete_update: Option<concrete::Update<T, CMake>>,
}

#[derive(Clone, Debug)]
enum TrafficSplit {
    Forward,
    Split(SmallRng),
}

#[derive(Clone, Debug)]
pub enum Concrete<T, M: Make<T>> {
    Forward(M::Service),
    Split(concrete::Service<M::Service>),
}

impl<G, RMake> Layer<G, RMake>
where
    G: GetRoutes + Clone,
    RMake: Clone,
{
    pub fn new(get_routes: G, make_route: RMake) -> Self {
        Self {
            get_routes,
            make_route,
            default_route: Route::default(),
            traffic_split: TrafficSplit::Forward,
        }
    }

    pub fn with_split(mut self) -> Self {
        self.traffic_split = TrafficSplit::Split(SmallRng::from_entropy());
        self
    }
}

impl<G, RMake, CMake> tower::layer::Layer<CMake> for Layer<G, RMake>
where
    G: GetRoutes + Clone,
    RMake: Clone,
{
    type Service = MakeSvc<G, RMake, CMake>;

    fn layer(&self, make_concrete: CMake) -> Self::Service {
        MakeSvc {
            make_concrete,
            get_routes: self.get_routes.clone(),
            make_route: self.make_route.clone(),
            default_route: self.default_route.clone(),
            traffic_split: self.traffic_split.clone(),
        }
    }
}

impl<T, G, RMake, CMake> Make<T> for MakeSvc<G, RMake, CMake>
where
    G: GetRoutes,
    T: CanGetDestination + WithRoute + WithAddr + Clone,
    RMake: Make<T::Output> + Clone,
    CMake: Make<T> + Clone,
    CMake::Service: Clone,
{
    type Service = Service<T, G::Stream, RMake, CMake>;

    fn make(&self, target: T) -> Self::Service {
        let profiles = match target.get_destination() {
            Some(ref dst) => self.get_routes.get_routes(&dst),
            None => {
                debug!("no destination for routes");
                None
            }
        };

        let requests = Requests::new(
            target.clone(),
            self.make_route.clone(),
            self.default_route.clone(),
        );
        let (concrete, concrete_update) = match self.traffic_split {
            TrafficSplit::Forward => {
                let service = self.make_concrete.make(target);
                (Concrete::Forward(service), None)
            }
            TrafficSplit::Split(ref rng) => {
                let (service, update) =
                    concrete::forward(target, self.make_concrete.clone(), rng.clone());
                (Concrete::Split(service), Some(update))
            }
        };
        Service {
            profiles,
            requests,
            concrete,
            concrete_update,
        }
    }
}

impl<T, P, RMake, CMake> Service<T, P, RMake, CMake>
where
    T: WithRoute + WithAddr + Clone,
    P: Stream<Item = Routes, Error = Never>,
    RMake: Make<T::Output>,
    CMake: Make<T>,
    CMake::Service: Clone,
{
    // Drive the profiles stream to notready or completion, capturing the
    // most recent update.
    fn poll_update(&mut self) {
        let mut profile = None;
        while let Some(Async::Ready(Some(update))) =
            self.profiles.as_mut().and_then(|ref mut s| s.poll().ok())
        {
            profile = Some(update);
        }

        if let Some(profile) = profile {
            if let Some(ref mut update) = self.concrete_update {
                if profile.dst_overrides.is_empty() {
                    update
                        .set_forward()
                        .expect("both sides of the concrete updater must be held");
                } else {
                    debug!(services = profile.dst_overrides.len(), "updating split");

                    update
                        .set_split(profile.dst_overrides)
                        .expect("both sides of the concrete updater must be held");
                }
            }

            debug!(routes = profile.routes.len(), "updating routes");
            self.requests.set_routes(profile.routes);
        }
    }
}

impl<B, T, P, RMake, CMake, R, C> tower::Service<http::Request<B>> for Service<T, P, RMake, CMake>
where
    T: WithRoute + WithAddr + Clone,
    P: Stream<Item = Routes, Error = Never>,
    CMake: Make<T, Service = C>,
    RMake: Make<T::Output, Service = R>,
    R: Proxy<http::Request<B>, Concrete<T, CMake>>,
    R::Error: Into<Error>,
    C: tower::Service<R::Request> + Clone,
    C::Error: Into<Error>,
{
    type Response = R::Response;
    type Error = Error;
    type Future = future::MapErr<R::Future, fn(R::Error) -> Error>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.poll_update();
        try_ready!(self.concrete.poll_ready());
        Ok(().into())
    }

    fn call(&mut self, req: http::Request<B>) -> Self::Future {
        self.requests
            .proxy(&mut self.concrete, req)
            .map_err(Into::into)
    }
}

impl<T, M, C, Req> tower::Service<Req> for Concrete<T, M>
where
    T: WithRoute + WithAddr + Clone,
    M: Make<T, Service = C>,
    C: tower::Service<Req> + Clone,
    C::Error: Into<Error>,
{
    type Response = C::Response;
    type Error = Error;
    type Future = future::MapErr<C::Future, fn(C::Error) -> Error>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        match self {
            Concrete::Forward(ref mut svc) => svc.poll_ready().map_err(Into::into),
            Concrete::Split(ref mut svc) => svc.poll_ready(),
        }
    }

    fn call(&mut self, req: Req) -> Self::Future {
        match self {
            Concrete::Forward(ref mut svc) => svc.call(req).map_err(Into::into),
            Concrete::Split(ref mut svc) => svc.call(req),
        }
    }
}
