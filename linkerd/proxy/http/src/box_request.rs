use super::boxed::Payload;
use futures::Poll;
use linkerd2_error::Error;

pub struct Layer<B>(std::marker::PhantomData<fn(B)>);

#[derive(Clone, Debug)]
pub struct BoxRequest<S, B>(S, std::marker::PhantomData<fn(B)>);

impl<B> Layer<B> {
    pub fn new() -> Self {
        Layer(std::marker::PhantomData)
    }
}

impl<B> Clone for Layer<B> {
    fn clone(&self) -> Self {
        Layer(self.0)
    }
}

impl<S, B> tower::layer::Layer<S> for Layer<B>
where
    S: tower::Service<http::Request<B>>,
{
    type Service = BoxRequest<S, B>;

    fn layer(&self, inner: S) -> Self::Service {
        BoxRequest(inner, self.0)
    }
}

impl<S, B> tower::Service<http::Request<B>> for BoxRequest<S, B>
where
    B: hyper::body::Payload + Send + 'static,
    B::Error: Into<Error> + 'static,
    S: tower::Service<http::Request<Payload<B::Data, B::Error>>>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.0.poll_ready()
    }

    fn call(&mut self, req: http::Request<B>) -> Self::Future {
        self.0.call(req.map(Payload::new))
    }
}
