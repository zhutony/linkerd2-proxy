use futures::{future, Future, Poll};
use linkerd2_error::Error;
use tower::util::BoxService;

pub type BoxForward<C> = BoxService<C, (), Error>;

pub type ForwardFuture = Box<dyn Future<Item = (), Error = Error> + Send + 'static>;

pub struct Layer<C>(std::marker::PhantomData<fn(C)>);

pub struct Make<C, M> {
    inner: M,
    _marker: std::marker::PhantomData<fn(C)>,
}

struct Inner<S> {
    service: S,
}

// === impl Layer ===

impl<C> Layer<C> {
    pub fn new() -> Self {
        Layer(std::marker::PhantomData)
    }
}

impl<C, M> tower::layer::Layer<M> for Layer<C> {
    type Service = Make<C, M>;

    fn layer(&self, inner: M) -> Self::Service {
        Self::Service {
            inner,
            _marker: self.0,
        }
    }
}

impl<C> Clone for Layer<C> {
    fn clone(&self) -> Self {
        Layer(self.0)
    }
}

// === impl Make ===

impl<T, C, M> tower::Service<T> for Make<C, M>
where
    M: tower::MakeService<T, C, Response = ()>,
    M::Error: Into<Error> + 'static,
    M::Service: Send + 'static,
    <M::Service as tower::Service<C>>::Future: Send + 'static,
{
    type Response = BoxForward<C>;
    type Error = M::MakeError;
    type Future = future::Map<M::Future, fn(M::Service) -> Self::Response>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.inner.poll_ready()
    }

    fn call(&mut self, target: T) -> Self::Future {
        self.inner.make_service(target).map(Inner::boxed)
    }
}

impl<C, M: Clone> Clone for Make<C, M> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            _marker: self._marker,
        }
    }
}

// === impl Inner ===

impl<S> Inner<S> {
    pub fn boxed<C>(service: S) -> BoxForward<C>
    where
        S: tower::Service<C, Response = ()> + Send + 'static,
        S::Error: Into<Error> + 'static,
        S::Future: Send + 'static,
    {
        BoxService::new(Inner { service })
    }
}

impl<C, S> tower::Service<C> for Inner<S>
where
    S: tower::Service<C, Response = ()>,
    S::Error: Into<Error> + 'static,
    S::Future: Send + 'static,
{
    type Response = ();
    type Error = Error;
    type Future = ForwardFuture;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.service.poll_ready().map_err(Into::into)
    }

    fn call(&mut self, conn: C) -> Self::Future {
        Box::new(self.service.call(conn).map_err(Into::into))
    }
}
