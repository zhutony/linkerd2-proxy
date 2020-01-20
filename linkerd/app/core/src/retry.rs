use super::classify;
use super::dst::Route;
use super::http_metrics::retries::Handle;
use super::HttpRouteRetry;
use crate::profiles;
use futures::future;
use linkerd2_http_classify::{Classify, ClassifyEos, ClassifyResponse};
use std::marker::PhantomData;
use std::sync::Arc;
use tower::retry::budget::Budget;

pub trait CloneRequest<Req> {
    fn clone_request(req: &Req) -> Option<Req>;
}

#[derive(Clone, Debug)]
pub struct NewRetry<C> {
    metrics: HttpRouteRetry,
    _clone_request: PhantomData<C>,
}

pub struct Retry<C> {
    metrics: Handle,
    budget: Arc<Budget>,
    response_classes: profiles::ResponseClasses,
    _clone_request: PhantomData<C>,
}

impl<C> NewRetry<C> {
    pub fn new(metrics: super::HttpRouteRetry) -> Self {
        NewRetry {
            metrics,
            _clone_request: PhantomData,
        }
    }
}

impl<C> linkerd2_retry::NewPolicy<Route> for NewRetry<C> {
    type Policy = Retry<C>;

    fn new_policy(&self, route: &Route) -> Option<Self::Policy> {
        let retries = route.route.retries().cloned()?;

        let metrics = self.metrics.get_handle(route.clone());
        Some(Retry {
            metrics,
            budget: retries.budget().clone(),
            response_classes: route.route.response_classes().clone(),
            _clone_request: self._clone_request,
        })
    }
}

impl<C, A, B, E> linkerd2_retry::Policy<http::Request<A>, http::Response<B>, E> for Retry<C>
where
    C: CloneRequest<http::Request<A>>,
{
    type Future = future::FutureResult<Self, ()>;

    fn retry(
        &self,
        req: &http::Request<A>,
        result: Result<&http::Response<B>, &E>,
    ) -> Option<Self::Future> {
        let retryable = match result {
            Err(_) => false,
            Ok(rsp) => classify::Request::from(self.response_classes.clone())
                .classify(req)
                .start(rsp)
                .eos(None)
                .is_failure(),
        };

        if !retryable {
            self.budget.deposit();
            return None;
        }

        let budget = self.budget.withdraw();
        self.metrics.incr_retryable(budget.is_ok());

        if budget.is_ok() {
            Some(future::ok(self.clone()))
        } else {
            None
        }
    }

    fn clone_request(&self, req: &http::Request<A>) -> Option<http::Request<A>> {
        C::clone_request(req)
    }
}

impl<C> Clone for Retry<C> {
    fn clone(&self) -> Self {
        Self {
            metrics: self.metrics.clone(),
            budget: self.budget.clone(),
            response_classes: self.response_classes.clone(),
            _clone_request: self._clone_request,
        }
    }
}
