use super::boxed::Payload;
use futures::Poll;
use linkerd2_error::Error;

#[derive(Clone, Debug)]
pub struct Layer(());

#[derive(Clone, Debug)]
pub struct BoxRequest<S>(S);

impl Layer {
    pub fn new() -> Self {
        Layer(())
    }
}

impl<S> tower::layer::Layer<S> for Layer {
    type Service = BoxRequest<S>;

    fn layer(&self, inner: S) -> Self::Service {
        BoxRequest(inner)
    }
}

impl<S, B> tower::Service<http::Request<B>> for BoxRequest<S>
where
    B: hyper::body::Payload + Send + 'static,
    B::Error: Into<Error> + 'static,
    S: tower::Service<http::Request<Payload>>,
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
