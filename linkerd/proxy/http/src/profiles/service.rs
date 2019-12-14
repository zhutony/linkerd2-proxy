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

use super::concrete::Concrete;
use super::requests::Requests;
use super::{CanGetDestination, GetRoutes, Route, Routes, WithAddr, WithRoute};
use futures::{future, try_ready, Async, Future, Poll, Stream};
use linkerd2_error::{Error, Never};
use linkerd2_router::Make;
use linkerd2_stack::Proxy;
use rand::{rngs::SmallRng, SeedableRng};
use tracing::debug;

pub fn layer<G, L>(get_routes: G, route_layer: L) -> Layer<G, L>
where
    G: GetRoutes + Clone,
    L: Clone,
{
    Layer {
        get_routes,
        route_layer,
        default_route: Route::default(),
        rng: SmallRng::from_entropy(),
    }
}

#[derive(Clone, Debug)]
pub struct Layer<G, L> {
    get_routes: G,
    route_layer: L,
    /// This is saved into a field so that the same `Arc`s are used and
    /// cloned, instead of calling `Route::default()` every time.
    default_route: Route,
    rng: SmallRng,
}

#[derive(Clone, Debug)]
pub struct MakeSvc<G, CMake, L> {
    get_routes: G,
    make_concrete: CMake,
    route_layer: L,
    default_route: Route,
    rng: SmallRng,
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
}

impl<G, CMake, L> tower::layer::Layer<CMake> for Layer<G, L>
where
    G: GetRoutes + Clone,
    L: Clone,
{
    type Service = MakeSvc<G, CMake, L>;

    fn layer(&self, make_concrete: CMake) -> Self::Service {
        MakeSvc {
            make_concrete,
            get_routes: self.get_routes.clone(),
            route_layer: self.route_layer.clone(),
            default_route: self.default_route.clone(),
            rng: self.rng.clone(),
        }
    }
}

impl<G, CMake, L, T> tower::Service<T> for MakeSvc<G, CMake, L>
where
    G: GetRoutes,
    T: CanGetDestination + WithRoute + WithAddr + Clone,
    CMake: Make<T> + Clone,
    L: tower::layer::Layer<()> + Clone,
    L::Service: Make<T::Output> + Clone,
{
    type Response = Service<T, G::Stream, L::Service, CMake>;
    type Error = Never;
    type Future = future::FutureResult<Self::Response, Self::Error>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        Ok(().into()) // always ready to make a Router
    }

    fn call(&mut self, target: T) -> Self::Future {
        let updates = match target.get_destination() {
            Some(ref dst) => self.get_routes.get_routes(&dst),
            None => {
                debug!("no destination for routes");
                None
            }
        };

        future::ok(Service::new(
            target,
            updates,
            self.route_layer.layer(()),
            self.default_route.clone(),
            self.make_concrete.clone(),
            self.rng.clone(),
        ))
    }
}

impl<T, P, RMake, CMake> Service<T, P, RMake, CMake>
where
    T: WithRoute + WithAddr + Clone,
    P: Stream<Item = Routes, Error = Never>,
    RMake: Make<T::Output>,
    CMake: Make<T>,
{
    fn new(
        target: T,
        profiles: Option<P>,
        make_route: RMake,
        default_route: Route,
        make_concrete: CMake,
        rng: SmallRng,
    ) -> Self {
        Self {
            profiles,
            requests: Requests::new(target.clone(), make_route, default_route),
            concrete: Concrete::forward(target, make_concrete, rng),
        }
    }

    // Drive the profiles stream to notready or completion, capturing the
    // most recent update.
    fn poll_update(&mut self) {
        let mut profile = None;
        while let Some(Async::Ready(Some(update))) =
            self.profiles.as_mut().and_then(|ref mut s| s.poll().ok())
        {
            profile = Some(update);
        }

        if let Some(update) = profile {
            if update.dst_overrides.is_empty() {
                self.concrete.set_forward();
            } else {
                self.concrete.set_split(update.dst_overrides);
            }

            self.requests.set_routes(update.routes);
        }
    }
}

impl<B, T, P, RMake, CMake, R, C> tower::Service<http::Request<B>> for Service<T, P, RMake, CMake>
where
    T: WithRoute + WithAddr + Clone,
    P: Stream<Item = Routes, Error = Never>,
    CMake: Make<T, Value = C>,
    RMake: Make<T::Output, Value = R>,
    R: Proxy<http::Request<B>, Concrete<T, CMake>>,
    R::Error: Into<Error>,
    C: tower::Service<R::Request>,
    C::Error: Into<Error>,
{
    type Response = R::Response;
    type Error = Error;
    type Future = future::MapErr<R::Future, fn(R::Error) -> Error>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        try_ready!(self.concrete.poll_ready().map_err(Into::into));
        // Don't bother updating routes until the inner service is ready.
        self.poll_update();
        Ok(().into())
    }

    fn call(&mut self, req: http::Request<B>) -> Self::Future {
        self.requests
            .proxy(&mut self.concrete, req)
            .map_err(Into::into)
    }
}
