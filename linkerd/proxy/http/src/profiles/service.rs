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
use super::{CanGetDestination, GetRoutes, Route, Routes, WeightedAddr, WithAddr, WithRoute};
use futures::{future, Async, Poll, Stream};
use indexmap::IndexMap;
use linkerd2_error::{Error, Never};
use linkerd2_router as rt;
use linkerd2_stack::{Proxy, Shared};
use std::hash::Hash;
use tracing::{debug, error};

pub fn layer<G, ConcreteMake, RouteLayer, RouteReq, ConcreteReq>(
    get_routes: G,
    route_layer: RouteLayer,
) -> Layer<G, ConcreteMake, RouteLayer, RouteReq, ConcreteReq>
where
    G: GetRoutes + Clone,
    RouteLayer: Clone,
{
    Layer {
        get_routes,
        route_layer,
        default_route: Route::default(),
        _p: ::std::marker::PhantomData,
    }
}

#[derive(Debug)]
pub struct Layer<G, ConcreteMake, RouteLayer, RouteReq, ConcreteReq> {
    get_routes: G,
    route_layer: RouteLayer,
    /// This is saved into a field so that the same `Arc`s are used and
    /// cloned, instead of calling `Route::default()` every time.
    default_route: Route,
    _p: ::std::marker::PhantomData<fn() -> (ConcreteMake, RouteReq, ConcreteReq)>,
}

#[derive(Debug)]
pub struct MakeSvc<G, ConcreteMake, RouteLayer, RouteReq, ConcreteReq> {
    make_concrete: ConcreteMake,
    get_routes: G,
    route_layer: RouteLayer,
    default_route: Route,
    _p: ::std::marker::PhantomData<fn(RouteReq, ConcreteReq)>,
}

/// The Service consists of a RouteRouter which routes over the route
/// stack built by the `route_layer`.  The per-route stack is terminated by
/// a shared `concrete_router`.  The `concrete_router` routes over the
/// underlying stack and passes the concrete dst as the target.
///
/// ```plain
///     +--------------+
///     |RouteRouter   | Target = t
///     +--------------+
///     |route_layer   | Target = t.with_route(route)
///     +--------------+
///     |ConcreteRouter| Target = t
///     +--------------+
///     |inner         | Target = t.with_addr(concrete_dst)
///     +--------------+
/// ```
pub struct Service<RouteStream, Target, RouteMake, ConcreteMake, RouteReq, ConcreteReq>
where
    Target: WithAddr + WithRoute + Clone + Eq + Hash,
    Target::Output: Clone + Eq + Hash,
    ConcreteMake: rt::Make<Target>,
    ConcreteMake::Value: tower::Service<ConcreteReq>,
    RouteMake: rt::Make<Target::Output>,
    RouteMake::Value: Proxy<RouteReq, Concrete<ConcreteMake::Value>>,
{
    target: Target,
    make_concrete: ConcreteMake,
    make_route: RouteMake,
    route_stream: Option<RouteStream>,
    requests: Requests<RouteMake::Value>,
    concrete: Concrete<ConcreteMake::Value>,
    default_route: Route,
}

impl<G, ConcreteMake, RouteLayer, RouteReq, ConcreteReq> tower::layer::Layer<ConcreteMake>
    for Layer<G, ConcreteMake, RouteLayer, RouteReq, ConcreteReq>
where
    G: GetRoutes + Clone,
    RouteLayer: Clone,
{
    type Service = MakeSvc<G, ConcreteMake, RouteLayer, RouteReq, ConcreteReq>;

    fn layer(&self, make_concrete: ConcreteMake) -> Self::Service {
        MakeSvc {
            make_concrete,
            get_routes: self.get_routes.clone(),
            route_layer: self.route_layer.clone(),
            default_route: self.default_route.clone(),
            _p: ::std::marker::PhantomData,
        }
    }
}

impl<G, ConcreteMake, RouteLayer, RouteReq, ConcreteReq> Clone
    for Layer<G, ConcreteMake, RouteLayer, RouteReq, ConcreteReq>
where
    G: Clone,
    RouteLayer: Clone,
{
    fn clone(&self) -> Self {
        Layer {
            get_routes: self.get_routes.clone(),
            route_layer: self.route_layer.clone(),
            default_route: self.default_route.clone(),
            _p: ::std::marker::PhantomData,
        }
    }
}

impl<G, ConcreteMake, RouteLayer, RouteReq, ConcreteReq, Target, RouteProxy> tower::Service<Target>
    for MakeSvc<G, ConcreteMake, RouteLayer, RouteReq, ConcreteReq>
where
    G: GetRoutes,
    Target: CanGetDestination + WithRoute + WithAddr + Eq + Hash + Clone,
    <Target as WithRoute>::Output: Eq + Hash + Clone,
    ConcreteMake: rt::Make<Target> + Clone,
    ConcreteMake::Value: tower::Service<ConcreteReq>,
    <ConcreteMake::Value as tower::Service<ConcreteReq>>::Error: Into<Error>,
    RouteLayer: tower::layer::Layer<()> + Clone,
    RouteLayer::Service: rt::Make<<Target as WithRoute>::Output, Value = RouteProxy> + Clone,
    RouteProxy: Proxy<RouteReq, Concrete<ConcreteMake::Value>>,
    RouteProxy::Error: Into<Error>,
{
    type Response = Service<G::Stream, Target, RouteProxy, ConcreteMake, RouteReq, ConcreteReq>;
    type Error = Never;
    type Future = future::FutureResult<Self::Response, Self::Error>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        Ok(().into()) // always ready to make a Router
    }

    fn call(&mut self, target: Target) -> Self::Future {
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
            self.make_concrete.clone(),
            self.route_layer.clone(),
        ))
    }
}

impl<G, ConcreteMake, RouteLayer, ConcreteReq, RouteReq> Clone
    for MakeSvc<G, ConcreteMake, RouteLayer, ConcreteReq, RouteReq>
where
    G: Clone,
    ConcreteMake: Clone,
    RouteLayer: Clone,
{
    fn clone(&self) -> Self {
        MakeSvc {
            make_concrete: self.make_concrete.clone(),
            get_routes: self.get_routes.clone(),
            route_layer: self.route_layer.clone(),
            default_route: self.default_route.clone(),
            _p: ::std::marker::PhantomData,
        }
    }
}

impl<RouteStream, Target, RouteMake, ConcreteMake, RouteReq, ConcreteReq>
    Service<RouteStream, Target, RouteMake, ConcreteMake, RouteReq, ConcreteReq>
where
    RouteStream: Stream<Item = Routes, Error = Never>,
    Target: WithRoute + WithAddr + Eq + Hash + Clone,
    Target::Output: Clone + Eq + Hash,
    RouteMake: rt::Make<Target::Output> + Clone,
    RouteMake::Value: Proxy<RouteReq, Concrete<ConcreteMake::Value>>,
    ConcreteMake: rt::Make<Target> + Clone,
    ConcreteMake::Value: tower::Service<ConcreteReq>,
{
    fn new(
        target: Target,
        updates: RouteStream,
        make_concrete: ConcreteMake,
        make_route: RouteMake,
        default_route: Route,
    ) -> Self {
        let concrete = Concrete::service(make_concrete.make(&target));

        // Initially there are no routes, so build a route router with only
        // the default route.
        let proxy_request = Requests::new(target.clone(), make_route, default_route);

        Self {
            concrete,
            proxy_request,
        }
    }

    fn update_routes(&mut self, routes: Routes) {
        // We must build a new concrete router with a service for each
        // dst_override.  These services are created eagerly.  If a service
        // was present in the previous concrete router, we reuse that
        // service in the new concrete router rather than recreating it.
        let capacity = routes.dst_overrides.len() + 1;

        let mut make = IndexMap::with_capacity(capacity);
        let mut old_routes = self
            .concrete_router
            .take()
            .expect("previous concrete dst router is missing")
            .into_routes();

        let target_svc = old_routes.remove(&self.target).unwrap_or_else(|| {
            error!("concrete dst router did not contain target dst");
            self.make_concrete.make(&self.target)
        });
        make.insert(self.target.clone(), target_svc);

        for WeightedAddr { addr, .. } in &routes.dst_overrides {
            let target = self.target.clone().with_addr(addr.clone());
            let service = old_routes
                .remove(&target)
                .unwrap_or_else(|| self.make_concrete.make(&target));
            make.insert(target, service);
        }

        let concrete_router = rt::Router::new_fixed(
            ConcreteDstRecognize::new(self.target.clone(), routes.dst_overrides),
            make,
        );

        // We store the concrete_router directly in the Service struct so
        // that we can extract its services when its time to construct a
        // new concrete router.
        self.concrete_router = Some(concrete_router.clone());

        let stack = self.route_layer.layer(Shared::new(concrete_router));

        let default_route = self.target.clone().with_route(self.default_route.clone());

        // Create a new fixed router router; we can eagerly make the
        // services and never expire the routes from the profile router
        // cache.
        let capacity = routes.routes.len() + 1;
        let mut make = IndexMap::with_capacity(capacity);
        make.insert(default_route.clone(), stack.make(&default_route));

        for (_, route) in &routes.routes {
            let route = self.target.clone().with_route(route.clone());
            let service = stack.make(&route);
            make.insert(route, service);
        }
    }
}

impl<RouteStream, Target, RouteMake, ConcreteMake, RouteReq, ConcreteReq, RouteProxy>
    tower::Service<RouteReq>
    for Service<RouteStream, Target, RouteMake, ConcreteMake, RouteReq, ConcreteReq>
where
    RouteStream: Stream<Item = Routes, Error = Never>,
    Target: WithRoute + WithAddr + Eq + Hash + Clone,
    Target::Output: Clone + Eq + Hash,
    ConcreteMake: rt::Make<Target> + Clone,
    ConcreteMake::Value: tower::Service<ConcreteReq>,
    RouteMake: rt::Make<Target::Output, Value = RouteProxy> + Clone,
    RouteProxy: Proxy<RouteReq, Concrete<ConcreteMake::Value>>,
{
    type Response = RouteProxy::Response;
    type Error = Error;
    type Future = future::MapErr<
        RouteProxy::Future,
        fn(<ConcreteMake::Value as tower::Service<ConcreteReq>>::Error) -> Error,
    >;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        while let Some(Async::Ready(Some(routes))) = self
            .route_stream
            .as_mut()
            .and_then(|ref mut s| s.poll().ok())
        {
            self.update_routes(routes);
        }

        self.concrete.poll_ready().map_err(Into::into)
    }

    fn call(&mut self, req: RouteReq) -> Self::Future {
        self.routes.proxy(&mut self.concrete, req)
    }
}
