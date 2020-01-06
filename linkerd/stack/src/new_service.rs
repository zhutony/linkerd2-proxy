use futures::{future, Poll};
use linkerd2_error::Never;

pub trait NewService<T> {
    type Service;

    fn new_service(&self, target: T) -> Self::Service;

    fn into_make_service(self) -> MakeService<Self>
    where
        Self: Sized,
    {
        MakeService(self)
    }
}

#[derive(Clone, Debug, Default)]
pub struct MakeService<N>(N);

// === impl NewService ===

/// Useful when building `Proxy` stacks.
impl<T> NewService<T> for () {
    type Service = ();

    fn new_service(&self, _: T) -> Self::Service {
        ()
    }
}

impl<F, T, S> NewService<T> for F
where
    F: Fn(T) -> S,
{
    type Service = S;

    fn new_service(&self, target: T) -> Self::Service {
        (self)(target)
    }
}

// === impl MakeService ===

impl<N: NewService<T>, T> tower::Service<T> for MakeService<N> {
    type Response = N::Service;
    type Error = Never;
    type Future = future::FutureResult<N::Service, Never>;

    fn poll_ready(&mut self) -> Poll<(), Never> {
        Ok(().into())
    }

    fn call(&mut self, target: T) -> Self::Future {
        future::ok(self.0.new_service(target))
    }
}
