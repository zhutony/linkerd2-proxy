use bytes::Bytes;
use futures::{future, Future, Poll};
use linkerd2_error::Error;
use tower::Service;

pub type Payload = Box<dyn hyper::body::Payload<Data = Bytes, Error = Error>>;
pub type Response = http::Response<Payload>;
pub type ResponseFuture = Box<dyn Future<Item = Response, Error = Error>>;

pub struct BoxedHttpService<A>(
    Box<dyn Service<http::Request<A>, Response = Response, Error = Error, Future = ResponseFuture>>,
);

pub struct Layer<A, B> {
    _marker: std::marker::PhantomData<fn(A) -> B>,
}

pub struct Make<M, A, B> {
    inner: M,
    _marker: std::marker::PhantomData<fn(A) -> B>,
}

impl<M, A, B> tower::layer::Layer<M> for Layer<A, B> {
    type Service = Make<M, A, B>;

    fn layer(&self, inner: M) -> Self::Service {
        Self::Service {
            inner,
            _marker: std::marker::PhantomData,
        }
    }
}

impl<T, M, A, B> Service<T> for Make<M, A, B>
where
    A: 'static,
    M: tower::MakeService<T, http::Request<A>, Response = http::Response<B>>,
    M::Service: 'static,
    M::Error: Into<Error> + 'static,
    B: Into<Payload> + 'static,
{
    type Response = BoxedHttpService<A>;
    type Error = M::MakeError;
    type Future = future::Map<M::Future, fn(M::Service) -> BoxedHttpService<A>>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.inner.poll_ready()
    }

    fn call(&mut self, target: T) -> Self::Future {
        self.inner.make_service(target).map(BoxedHttpService::new)
    }
}

struct Inner<S, A, B> {
    service: S,
    _marker: std::marker::PhantomData<fn(A) -> B>,
}

impl<A: 'static> BoxedHttpService<A> {
    fn new<B, E>(
        service: impl Service<http::Request<A>, Response = http::Response<B>, Error = E> + 'static,
    ) -> Self
    where
        B: Into<Payload> + 'static,
        E: Into<Error> + 'static,
    {
        BoxedHttpService(Box::new(Inner {
            service,
            _marker: std::marker::PhantomData,
        }))
    }
}

impl<A> Service<http::Request<A>> for BoxedHttpService<A> {
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

impl<S, A, B> Service<http::Request<A>> for Inner<S, A, B>
where
    S: Service<http::Request<A>, Response = http::Response<B>>,
    S::Error: Into<Error> + 'static,
    S::Future: 'static,
    B: Into<Payload> + 'static,
{
    type Response = Response;
    type Error = Error;
    type Future = ResponseFuture;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.service.poll_ready().map_err(Into::into)
    }

    fn call(&mut self, req: http::Request<A>) -> Self::Future {
        let fut = self
            .service
            .call(req)
            .map(|rsp| rsp.map(Into::into))
            .map_err(Into::into);
        Box::new(fut)
    }
}
