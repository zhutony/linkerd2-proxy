use super::boxed::{Data, Payload};
use futures::Poll;
use linkerd2_error::Error;

#[derive(Copy, Clone, Debug)]
pub struct Layer;

#[derive(Clone, Debug)]
pub struct BoxRequest<S>(S);

impl<S> tower::layer::Layer<S> for Layer {
    type Service = BoxRequest<S>;

    fn layer(&self, inner: S) -> Self::Service {
        BoxRequest(inner)
    }
}

impl<S, B> tower::Service<http::Request<B>> for BoxRequest<S>
where
    S: tower::Service<http::Request<Payload>>,
    B: hyper::body::Payload<Data = Data, Error = Error> + Send + 'static,
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
