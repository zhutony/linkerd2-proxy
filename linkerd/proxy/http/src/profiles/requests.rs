use super::{RequestMatch, Route};
use linkerd2_stack::{proxy, Proxy};
use tracing::trace;

#[derive(Clone)]
pub struct Requests<P> {
    routes: Vec<(RequestMatch, Route, P)>,
    default: P,
}

impl<P: Clone> Requests<P> {
    pub fn new(default: P) -> Self {
        Self {
            default,
            routes: Vec::default(),
        }
    }
}

impl<B, P, S> proxy::Route<http::Request<B>, S> for Requests<P>
where
    P: Proxy<http::Request<B>, S> + Clone,
    S: tower::Service<P::Request>,
{
    type Request = P::Request;
    type Response = P::Response;
    type Error = P::Error;
    type Future = P::Future;
    type Proxy = P;

    fn route<'a>(&mut self, req: &'a http::Request<B>) -> Self::Proxy {
        for (ref condition, ref route) in &self.routes {
            if condition.is_match(&req) {
                trace!("using configured route: {:?}", condition);
                return route.clone();
            }
        }

        self.default.clone()
    }
}
