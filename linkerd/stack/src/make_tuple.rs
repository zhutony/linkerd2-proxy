use futures::{try_ready, Future, Poll};
use linkerd2_error::Error;

#[derive(Clone, Debug)]
pub struct Layer(());

#[derive(Clone, Debug)]
pub struct Make<S> {
    inner: S,
}

#[derive(Clone, Debug)]
pub struct Service<S> {
    inner: S,
}

pub enum ResponseFuture<F, Req>
where
    F: Future,
    F::Item: tower::Service<Req>,
{
    Make(F, Option<Req>),
    Serve(tower_util::Oneshot<F::Item, Req>),
}

impl Layer {
    pub fn new() -> Self {
        Layer(())
    }
}

impl<S> tower::layer::Layer<S> for Layer {
    type Service = Make<S>;

    fn layer(&self, inner: S) -> Self::Service {
        Make { inner }
    }
}

impl<T, R, S> tower::Service<(T, R)> for Make<S>
where
    S: tower::MakeService<T, R>,
    S::MakeError: Into<Error>,
    S::Error: Into<Error>,
{
    type Response = S::Response;
    type Error = Error;
    type Future = ResponseFuture<S::Future, R>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.inner.poll_ready().map_err(Into::into)
    }

    fn call(&mut self, (target, req): (T, R)) -> Self::Future {
        ResponseFuture::Make(self.inner.make_service(target), Some(req))
    }
}

impl<F, I> Future for ResponseFuture<F, I>
where
    F: Future,
    F::Item: tower::Service<I>,
    F::Error: Into<Error>,
    <F::Item as tower::Service<I>>::Error: Into<Error>,
{
    type Item = <F::Item as tower::Service<I>>::Response;
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        loop {
            *self = match self {
                ResponseFuture::Make(ref mut fut, ref mut req) => {
                    let svc = try_ready!(fut.poll().map_err(Into::into));

                    let req = req.take().expect("illegal state");
                    ResponseFuture::Serve(tower_util::Oneshot::new(svc, req))
                }
                ResponseFuture::Serve(ref mut fut) => {
                    return fut.poll().map_err(Into::into);
                }
            }
        }
    }
}
