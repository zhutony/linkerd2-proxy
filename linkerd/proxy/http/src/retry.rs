use crate::metrics::{handle_time, Scoped, Stats};
use futures::{future, try_ready, Future, Poll};
use http::{Request, Response};
use linkerd2_error::Error;
use linkerd2_proxy_transport::tls;
use linkerd2_stack::{proxy, Make, Proxy};
use tower::retry;
pub use tower::retry::budget::Budget;
pub use tower::util::{Oneshot, ServiceExt};
use tracing::trace;

pub trait CanRetry {
    type Retry: Retry + Clone;
    fn can_retry(&self) -> Option<Self::Retry>;
}

pub trait Retry: Sized {
    fn retry<B1, B2>(&self, req: &Request<B1>, res: &Response<B2>) -> Result<(), NoRetry>;
    fn clone_request<B: TryClone>(&self, req: &Request<B>) -> Option<Request<B>>;
}

pub enum NoRetry {
    Success,
    Budget,
}

pub trait TryClone: Sized {
    fn try_clone(&self) -> Option<Self>;
}

#[derive(Clone, Debug)]
pub struct Layer<R> {
    registry: R,
}

#[derive(Clone, Debug)]
pub struct Stack<M, R> {
    registry: R,
    inner: M,
}

pub struct MakeFuture<F, R> {
    inner: F,
    policy: Option<R>,
}

pub enum Service<R, S> {
    Inner(S),
    Retry(R, S),
}

pub enum ResponseFuture<R, P, S, Req>
where
    R: retry::Policy<Req, P::Response, Error> + Clone,
    P: Proxy<Req, S> + Clone,
    S: tower::Service<P::Request> + Clone,
    S::Error: Into<Error>,
{
    Inner(P::Future),
    Retry(Oneshot<retry::Retry<R, proxy::Service<P, S>>, Req>),
}

#[derive(Clone)]
pub struct Policy<R, S>(R, S);

// === impl Layer ===

pub fn layer<R>(registry: R) -> Layer<R> {
    Layer { registry }
}

impl<M, R: Clone> tower::layer::Layer<M> for Layer<R> {
    type Service = Stack<M, R>;

    fn layer(&self, inner: M) -> Self::Service {
        Stack {
            inner,
            registry: self.registry.clone(),
        }
    }
}

// === impl Stack ===

impl<T, M, R> Make<T> for Stack<M, R>
where
    T: CanRetry + Clone,
    M: Make<T>,
    M::Service: Clone,
    R: Scoped<T>,
    R::Scope: Clone,
{
    type Service = Service<Policy<T::Retry, R::Scope>, M::Service>;

    fn make(&self, target: T) -> Self::Service {
        if let Some(retries) = target.can_retry() {
            trace!("stack is retryable");
            let policy = Policy(retries, self.registry.scoped(target.clone()));
            Service::Retry(policy, self.inner.make(target))
        } else {
            Service::Inner(self.inner.make(target))
        }
    }
}

impl<T, M, R> tower::Service<T> for Stack<M, R>
where
    T: CanRetry + Clone,
    M: tower::Service<T>,
    M::Response: Clone,
    R: Scoped<T>,
    R::Scope: Clone,
{
    type Response = Service<Policy<T::Retry, R::Scope>, M::Response>;
    type Error = M::Error;
    type Future = MakeFuture<M::Future, Policy<T::Retry, R::Scope>>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.inner.poll_ready()
    }

    fn call(&mut self, target: T) -> Self::Future {
        let policy = if let Some(retries) = target.can_retry() {
            trace!("stack is retryable");
            let stats = self.registry.scoped(target.clone());
            Some(Policy(retries, stats))
        } else {
            None
        };

        let inner = self.inner.call(target);
        MakeFuture { inner, policy }
    }
}

// === impl MakeFuture ===

impl<F, R> Future for MakeFuture<F, R>
where
    F: Future,
{
    type Item = Service<R, F::Item>;
    type Error = F::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let inner = try_ready!(self.inner.poll());
        match self.policy.take() {
            None => Ok(Service::Inner(inner).into()),
            Some(policy) => Ok(Service::Retry(policy, inner).into()),
        }
    }
}

// === impl Policy ===

impl<R, S, A, B, E> retry::Policy<Request<A>, Response<B>, E> for Policy<R, S>
where
    R: Retry + Clone,
    S: Stats + Clone,
    A: TryClone,
{
    type Future = future::FutureResult<Self, ()>;

    fn retry(&self, req: &Request<A>, result: Result<&Response<B>, &E>) -> Option<Self::Future> {
        match result {
            Ok(res) => match self.0.retry(req, res) {
                Ok(()) => {
                    trace!("retrying request");
                    Some(future::ok(self.clone()))
                }
                Err(NoRetry::Budget) => {
                    self.1.incr_retry_skipped_budget();
                    None
                }
                Err(NoRetry::Success) => None,
            },
            Err(_err) => {
                trace!("cannot retry transport error");
                None
            }
        }
    }

    fn clone_request(&self, req: &Request<A>) -> Option<Request<A>> {
        if let Some(clone) = self.0.clone_request(req) {
            trace!("cloning request");
            Some(clone)
        } else {
            trace!("request could not be cloned");
            None
        }
    }
}

// === impl Service ===

impl<R, Req, S, P> Proxy<Req, S> for Service<R, P>
where
    R: retry::Policy<Req, P::Response, Error> + Clone,
    P: Proxy<Req, S> + Clone,
    S: tower::Service<P::Request> + Clone,
    S::Error: Into<Error>,
{
    type Request = P::Request;
    type Response = P::Response;
    type Error = Error;
    type Future = ResponseFuture<R, P, S, Req>;

    fn proxy(&self, svc: &mut S, req: Req) -> Self::Future {
        match self {
            Service::Inner(ref p) => ResponseFuture::Inner(p.proxy(svc, req)),
            Service::Retry(ref policy, ref p) => {
                let svc = p.clone().into_service(svc.clone());
                let retry = retry::Retry::new(policy.clone(), svc);
                ResponseFuture::Retry(retry.oneshot(req))
            }
        }
    }
}

impl<R, P, S, Req> Future for ResponseFuture<R, P, S, Req>
where
    R: retry::Policy<Req, P::Response, Error> + Clone,
    P: Proxy<Req, S> + Clone,
    S: tower::Service<P::Request> + Clone,
    S::Error: Into<Error>,
{
    type Item = P::Response;
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        match self {
            ResponseFuture::Inner(ref mut f) => f.poll().map_err(Into::into),
            ResponseFuture::Retry(ref mut f) => f.poll().map_err(Into::into),
        }
    }
}

impl<B: TryClone> TryClone for Request<B> {
    fn try_clone(&self) -> Option<Self> {
        if let Some(body) = self.body().try_clone() {
            let mut clone = Request::new(body);
            *clone.method_mut() = self.method().clone();
            *clone.uri_mut() = self.uri().clone();
            *clone.headers_mut() = self.headers().clone();
            *clone.version_mut() = self.version();

            if let Some(ext) = self.extensions().get::<tls::accept::Meta>() {
                clone.extensions_mut().insert(ext.clone());
            }

            // Count retries toward the request's total handle time.
            if let Some(ext) = self.extensions().get::<handle_time::Tracker>() {
                clone.extensions_mut().insert(ext.clone());
            }

            Some(clone)
        } else {
            None
        }
    }
}
