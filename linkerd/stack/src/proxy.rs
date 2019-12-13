use futures::{future, Future, Poll};
use linkerd2_error::Error;
use tower_service as tower;

pub trait Proxy<Req, S: tower::Service<Self::Request>> {
    type Request;
    type Response;
    type Error: Into<Error>;
    type Future: Future<Item = Self::Response, Error = Self::Error>;

    fn proxy(&mut self, inner: &mut S, req: Req) -> Self::Future;

    fn into_service(self, inner: S) -> Service<Self, S>
    where
        Self: Sized,
    {
        Service { proxy: self, inner }
    }
}

pub trait Route<Req, S: tower::Service<Self::Request>> {
    type Request;
    type Response;
    type Error: Into<Error>;
    type Future: Future<Item = Self::Response, Error = Self::Error>;
    type Proxy: Proxy<
        Req,
        S,
        Request = Self::Request,
        Response = Self::Response,
        Error = Self::Error,
        Future = Self::Future,
    >;

    fn route<'a>(&mut self, req: &'a Req) -> Self::Proxy;

    fn into_proxy(self) -> RouteProxy<Self>
    where
        Self: Sized,
    {
        RouteProxy(self)
    }
}

pub struct Service<P, S> {
    proxy: P,
    inner: S,
}

pub struct RouteProxy<R>(R);

impl<Req, S> Proxy<Req, S> for ()
where
    S: tower::Service<Req>,
    S::Error: Into<Error>,
{
    type Request = Req;
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn proxy(&mut self, inner: &mut S, req: Req) -> Self::Future {
        inner.call(req)
    }
}

impl<Req, S, R> Proxy<Req, S> for RouteProxy<R>
where
    R: Route<Req, S>,
    S: tower::Service<R::Request>,
{
    type Request = R::Request;
    type Response = R::Response;
    type Error = R::Error;
    type Future = R::Future;

    fn proxy(&mut self, inner: &mut S, req: Req) -> Self::Future {
        self.0.route(&req).proxy(inner, req)
    }
}

impl<P, S> Service<P, S> {
    pub fn proxy(&self) -> &P {
        &self.proxy
    }

    pub fn proxy_mut(&mut self) -> &mut P {
        &mut self.proxy
    }

    pub fn inner(&self) -> &S {
        &self.inner
    }

    pub fn inner_mut(&mut self) -> &mut S {
        &mut self.inner
    }
}

impl<Req, P, S> tower::Service<Req> for Service<P, S>
where
    P: Proxy<Req, S>,
    S: tower::Service<P::Request>,
    S::Error: Into<Error>,
{
    type Response = P::Response;
    type Error = Error;
    type Future = future::MapErr<P::Future, fn(P::Error) -> Error>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.inner.poll_ready().map_err(Into::into)
    }

    fn call(&mut self, req: Req) -> Self::Future {
        self.proxy.proxy(&mut self.inner, req).map_err(Into::into)
    }
}
