use super::{RequestMatch, Route, WithRoute};
use linkerd2_router::Make;
use linkerd2_stack::{proxy, Proxy};
use tracing::trace;

/// A proxy that applies per-request "routes" over a common inner service.
#[derive(Clone, Debug, Default)]
pub struct Requests<T: WithRoute, M: Make<T::Output>> {
    target: T,
    make: M,
    default: M::Value,
    routes: Vec<(RequestMatch, M::Value)>,
}

impl<T, M> Requests<T, M>
where
    T: Clone + WithRoute,
    M: Make<T::Output>,
{
    pub fn new(target: T, make: M, default: Route) -> Self {
        let default = {
            let t = target.clone().with_route(default);
            make.make(&t)
        };
        Self {
            target,
            make,
            default,
            routes: Vec::default(),
        }
    }

    pub fn set_routes(&mut self, routes: Vec<(RequestMatch, Route)>) {
        self.routes = routes
            .into_iter()
            .map(|(cond, r)| {
                let t = self.target.clone().with_route(r);
                (cond, self.make.make(&t))
            })
            .collect();
    }
}

impl<T, M, P, B, S> proxy::Proxy<http::Request<B>, S> for Requests<T, M>
where
    T: WithRoute,
    M: Make<T::Output, Value = P>,
    P: Proxy<http::Request<B>, S>,
    S: tower::Service<P::Request>,
{
    type Request = P::Request;
    type Response = P::Response;
    type Error = P::Error;
    type Future = P::Future;

    fn proxy(&self, inner: &mut S, req: http::Request<B>) -> Self::Future {
        for (ref condition, ref route) in &self.routes {
            if condition.is_match(&req) {
                trace!(?condition, "using configured route");
                return route.proxy(inner, req);
            }
        }

        self.default.proxy(inner, req)
    }
}
