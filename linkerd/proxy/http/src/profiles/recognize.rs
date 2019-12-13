use super::{RequestMatch, Route, WithRoute};
use http;
use linkerd2_router as rt;
use std::hash::Hash;
use tracing::trace;

#[derive(Clone)]
pub struct RouteRecognize<T> {
    target: T,
    routes: Vec<(RequestMatch, Route)>,
    default_route: Route,
}

impl<T> RouteRecognize<T> {
    pub fn new(target: T, routes: Vec<(RequestMatch, Route)>, default_route: Route) -> Self {
        RouteRecognize {
            target,
            routes,
            default_route,
        }
    }
}

impl<Body, T> rt::Recognize<http::Request<Body>> for RouteRecognize<T>
where
    T: WithRoute + Clone,
    T::Output: Clone + Eq + Hash,
{
    type Target = T::Output;

    fn recognize(&self, req: &http::Request<Body>) -> Option<Self::Target> {
        for (ref condition, ref route) in &self.routes {
            if condition.is_match(&req) {
                trace!("using configured route: {:?}", condition);
                return Some(self.target.clone().with_route(route.clone()));
            }
        }

        trace!("using default route");
        Some(self.target.clone().with_route(self.default_route.clone()))
    }
}
