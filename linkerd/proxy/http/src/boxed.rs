use futures::{future, try_ready, Future, Poll};
use linkerd2_error::Error;

pub type Data = hyper::body::Chunk;
pub type Response = http::Response<Payload>;
pub type ResponseFuture = Box<dyn Future<Item = Response, Error = Error> + 'static>;

pub struct BoxedService<A>(
    Box<
        dyn tower::Service<
            http::Request<A>,
            Response = Response,
            Error = Error,
            Future = ResponseFuture,
        >,
    >,
);

pub struct BoxedCloneService<A>(Box<dyn CloneService<A>>);

trait CloneService<A> {
    fn clone(&self) -> Self
    where
        Self: Sized;

    fn poll_ready(&mut self) -> Poll<(), Error>;

    fn call(&mut self, req: http::Request<A>) -> ResponseFuture;
}

impl<S, A> CloneService<A> for S
where
    S: tower::Service<
            http::Request<A>,
            Response = Response,
            Error = Error,
            Future = ResponseFuture,
        > + Clone,
{
    fn clone(&self) -> Self
    where
        Self: Sized,
    {
        Clone::clone(self)
    }

    fn poll_ready(&mut self) -> Poll<(), Error> {
        tower::Service::poll_ready(self)
    }

    fn call(&mut self, req: http::Request<A>) -> ResponseFuture {
        tower::Service::call(self, req)
    }
}

pub struct BoxedVariant(());
pub struct BoxedCloneVariant(());

pub type LayerBoxed<A, B> = Layer<A, B, BoxedVariant>;
pub type LayerBoxedClone<A, B> = Layer<A, B, BoxedCloneVariant>;

pub struct Layer<A, B, V> {
    _marker: std::marker::PhantomData<fn(A, V) -> B>,
}

pub type MakeBoxed<M, A, B> = Make<M, A, B, BoxedVariant>;
pub type MakeBoxedClone<M, A, B> = Make<M, A, B, BoxedCloneVariant>;

pub struct Make<M, A, B, V> {
    inner: M,
    _marker: std::marker::PhantomData<fn(A, V) -> B>,
}

impl<A, B, V> Clone for Layer<A, B, V> {
    fn clone(&self) -> Self {
        Self {
            _marker: self._marker,
        }
    }
}

impl<M: Clone, A, B, V> Clone for Make<M, A, B, V> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            _marker: self._marker,
        }
    }
}

impl<A, B> Layer<A, B, BoxedVariant>
where
    A: 'static,
    B: hyper::body::Payload<Data = Data, Error = Error> + 'static,
{
    pub fn boxed() -> Self {
        Layer {
            _marker: std::marker::PhantomData,
        }
    }
}

impl<A, B> Layer<A, B, BoxedCloneVariant>
where
    A: 'static,
    B: hyper::body::Payload<Data = Data, Error = Error> + 'static,
{
    pub fn boxed_clone() -> Self {
        Layer {
            _marker: std::marker::PhantomData,
        }
    }
}

impl<M, A, B, V> tower::layer::Layer<M> for Layer<A, B, V> {
    type Service = Make<M, A, B, V>;

    fn layer(&self, inner: M) -> Self::Service {
        Self::Service {
            inner,
            _marker: std::marker::PhantomData,
        }
    }
}

impl<T, M, A, B> tower::Service<T> for Make<M, A, B, BoxedVariant>
where
    A: 'static,
    M: tower::MakeService<T, http::Request<A>, Response = http::Response<B>>,
    M::Error: Into<Error> + 'static,
    M::Service: 'static,
    B: hyper::body::Payload<Data = Data, Error = Error> + 'static,
{
    type Response = BoxedService<A>;
    type Error = M::MakeError;
    type Future = future::Map<M::Future, fn(M::Service) -> BoxedService<A>>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.inner.poll_ready()
    }

    fn call(&mut self, target: T) -> Self::Future {
        self.inner.make_service(target).map(BoxedService::new)
    }
}

impl<T, M, A, B> tower::Service<T> for Make<M, A, B, BoxedCloneVariant>
where
    A: 'static,
    M: tower::MakeService<T, http::Request<A>, Response = http::Response<B>>,
    M::Error: Into<Error> + 'static,
    M::Service: Clone + 'static,
    B: hyper::body::Payload<Data = Data, Error = Error> + 'static,
{
    type Response = BoxedCloneService<A>;
    type Error = M::MakeError;
    type Future = future::Map<M::Future, fn(M::Service) -> BoxedCloneService<A>>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.inner.poll_ready()
    }

    fn call(&mut self, target: T) -> Self::Future {
        self.inner.make_service(target).map(BoxedCloneService::new)
    }
}

struct Inner<S, A, B> {
    service: S,
    _marker: std::marker::PhantomData<fn(A) -> B>,
}

struct InnerFuture<F, B> {
    future: F,
    _marker: std::marker::PhantomData<fn() -> B>,
}

pub struct Payload {
    inner: Box<dyn hyper::body::Payload<Data = Data, Error = Error> + 'static>,
}

impl<A: 'static> BoxedService<A> {
    fn new<S, B>(service: S) -> Self
    where
        S: tower::Service<http::Request<A>, Response = http::Response<B>> + 'static,
        S::Error: Into<Error> + 'static,
        B: hyper::body::Payload<Data = Data, Error = Error> + 'static,
    {
        BoxedService(Box::new(Inner {
            service,
            _marker: std::marker::PhantomData,
        }))
    }
}

impl<A> tower::Service<http::Request<A>> for BoxedService<A> {
    type Response = Response;
    type Error = Error;
    type Future = ResponseFuture;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.0.poll_ready()
    }

    fn call(&mut self, req: http::Request<A>) -> Self::Future {
        self.0.call(req)
    }
}

impl<A: 'static> BoxedCloneService<A> {
    fn new<S, B>(service: S) -> Self
    where
        S: tower::Service<http::Request<A>, Response = http::Response<B>> + Clone + 'static,
        S::Error: Into<Error> + 'static,
        B: hyper::body::Payload<Data = Data, Error = Error> + 'static,
    {
        BoxedCloneService(Box::new(Inner {
            service,
            _marker: std::marker::PhantomData,
        }))
    }
}

impl<A> tower::Service<http::Request<A>> for BoxedCloneService<A> {
    type Response = Response;
    type Error = Error;
    type Future = ResponseFuture;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.0.poll_ready()
    }

    fn call(&mut self, req: http::Request<A>) -> Self::Future {
        self.0.call(req)
    }
}

impl<S, A, B> tower::Service<http::Request<A>> for Inner<S, A, B>
where
    S: tower::Service<http::Request<A>, Response = http::Response<B>>,
    S::Error: Into<Error> + 'static,
    S::Future: 'static,
    B: hyper::body::Payload<Data = Data, Error = Error> + 'static,
{
    type Response = Response;
    type Error = Error;
    type Future = ResponseFuture;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.service.poll_ready().map_err(Into::into)
    }

    fn call(&mut self, req: http::Request<A>) -> Self::Future {
        let future = self.service.call(req);
        Box::new(InnerFuture {
            future,
            _marker: std::marker::PhantomData,
        })
    }
}

impl<F, B> Future for InnerFuture<F, B>
where
    F: Future<Item = http::Response<B>>,
    F::Error: Into<Error>,
    B: hyper::body::Payload<Data = Data, Error = Error> + 'static,
{
    type Item = Response;
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let rsp: http::Response<B> = try_ready!(self.future.poll().map_err(Into::into));
        let rsp: Response = rsp.map(|inner| Payload {
            inner: Box::new(inner),
        });
        Ok(rsp.into())
    }
}

impl hyper::body::Payload for Payload {
    type Data = Data;
    type Error = Error;

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn poll_data(&mut self) -> Poll<Option<Self::Data>, Self::Error> {
        self.inner.poll_data().map_err(Into::into)
    }

    fn poll_trailers(&mut self) -> Poll<Option<http::HeaderMap>, Self::Error> {
        self.inner.poll_trailers().map_err(Into::into)
    }
}

impl<S: Clone, A, B> Clone for Inner<S, A, B> {
    fn clone(&self) -> Self {
        Self {
            service: self.service.clone(),
            _marker: self._marker,
        }
    }
}
