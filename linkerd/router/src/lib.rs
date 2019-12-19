use futures::{try_ready, Future, Poll};
use linkerd2_error::Error;

pub trait Target<Req> {
    type Target;

    fn target(&self, req: &Req) -> Self::Target;
}

pub struct Service<T, M> {
    target: T,
    make: M,
}

pub struct ResponseFuture<T, Req, M: tower::MakeService<T, Req>> {
    inner: Inner<Req, M::Future, M::Service, <M::Service as tower::Service<Req>>::Future>,
    _marker: std::marker::PhantomData<fn(T)>,
}

pub enum Inner<Req, M, S, F> {
    Make(M, Option<Req>),
    Ready(S, Option<Req>),
    Respond(F),
}

impl<Req, T, M> tower::Service<Req> for Service<T, M>
where
    T: Target<Req>,
    M: tower::MakeService<T::Target, Req>,
    M::Service: Clone,
    M::Error: Into<Error>,
    M::MakeError: Into<Error>,
{
    type Response = M::Response;
    type Error = Error;
    type Future = ResponseFuture<T::Target, Req, M>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.make.poll_ready().map_err(Into::into)
    }

    fn call(&mut self, req: Req) -> Self::Future {
        let target = self.target.target(&req);
        ResponseFuture {
            inner: Inner::Make(self.make.make_service(target), Some(req)),
            _marker: std::marker::PhantomData,
        }
    }
}

impl<T, Req, M> Future for ResponseFuture<T, Req, M>
where
    M: tower::MakeService<T, Req>,
    M::Service: Clone,
    M::Error: Into<Error>,
    M::MakeError: Into<Error>,
{
    type Item = M::Response;
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        use tower::Service;

        loop {
            self.inner = match self.inner {
                Inner::Make(ref mut fut, ref mut req) => {
                    let svc = try_ready!(fut.poll().map_err(Into::into));
                    Inner::Ready(svc, req.take())
                }
                Inner::Ready(ref mut svc, ref mut req) => {
                    try_ready!(svc.poll_ready().map_err(Into::into));
                    let req = req.take().expect("polled after ready");
                    Inner::Respond(svc.call(req))
                }
                Inner::Respond(ref mut fut) => return fut.poll().map_err(Into::into),
            }
        }
    }
}
