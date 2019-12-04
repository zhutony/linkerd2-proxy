use futures::{try_ready, Future, Poll};

#[derive(Clone, Debug)]
pub struct Layer<I, O>(fn(I) -> O);

#[derive(Debug)]
pub struct Make<S, I, O> {
    inner: S,
    map_request: fn(I) -> O,
}

#[derive(Debug)]
pub struct MakeFuture<F, I, O> {
    inner: F,
    map_request: fn(I) -> O,
}

#[derive(Debug)]
pub struct MapRequest<S, I, O> {
    inner: S,
    map_request: fn(I) -> O,
}

impl<I, O> Layer<I, O> {
    pub fn new(f: fn(I) -> O) -> Self {
        Layer(f)
    }
}

impl<S, I, O> tower::layer::Layer<S> for Layer<I, O> {
    type Service = Make<S, I, O>;

    fn layer(&self, inner: S) -> Self::Service {
        Make {
            inner,
            map_request: self.0,
        }
    }
}

impl<T, S, I, O> tower::Service<T> for Make<S, I, O>
where
    S: tower::MakeService<T, O>,
{
    type Response = MapRequest<S::Service, I, O>;
    type Error = S::MakeError;
    type Future = MakeFuture<S::Future, I, O>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.inner.poll_ready()
    }

    fn call(&mut self, target: T) -> Self::Future {
        MakeFuture {
            inner: self.inner.make_service(target),
            map_request: self.map_request,
        }
    }
}

impl<S: Clone, I, O> Clone for Make<S, I, O> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            map_request: self.map_request,
        }
    }
}

impl<F: Future, I, O> Future for MakeFuture<F, I, O> {
    type Item = MapRequest<F::Item, I, O>;
    type Error = F::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let inner = try_ready!(self.inner.poll());
        let svc = MapRequest {
            inner,
            map_request: self.map_request,
        };
        Ok(svc.into())
    }
}

impl<S, I, O> tower::Service<I> for MapRequest<S, I, O>
where
    S: tower::Service<O>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.inner.poll_ready()
    }

    fn call(&mut self, req: I) -> Self::Future {
        self.inner.call((self.map_request)(req))
    }
}

impl<S: Clone, I, O> Clone for MapRequest<S, I, O> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            map_request: self.map_request,
        }
    }
}
