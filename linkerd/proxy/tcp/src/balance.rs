use futures::{future, Future, Poll};
use linkerd2_error::Error;
use rand::{rngs::SmallRng, SeedableRng};
use tower_balance::p2c::Balance;
use tower_discover::Discover;
use tower_load::{NoInstrument, PendingRequestsDiscover};

/// Configures a stack to resolve `T` typed targets to balance requests over
/// `M`-typed endpoint stacks.
pub struct Layer<I> {
    _marker: std::marker::PhantomData<fn(I)>,
}

/// Resolves `T` typed targets to balance requests over `M`-typed endpoint stacks.
pub struct MakeSvc<M, I> {
    inner: M,
    _marker: std::marker::PhantomData<fn(I)>,
}

// === impl Layer ===

impl<I> Layer<I> {
    pub fn new() -> Self {
        Layer {
            _marker: std::marker::PhantomData,
        }
    }
}

impl<M, I> tower::layer::Layer<M> for Layer<I> {
    type Service = MakeSvc<M, I>;

    fn layer(&self, inner: M) -> Self::Service {
        MakeSvc {
            inner,
            _marker: self._marker,
        }
    }
}

impl<I> Clone for Layer<I> {
    fn clone(&self) -> Self {
        Self {
            _marker: self._marker,
        }
    }
}

// === impl MakeSvc ===

impl<T, M, I> tower::Service<T> for MakeSvc<M, I>
where
    M: tower::Service<T>,
    M::Response: Discover,
    <M::Response as Discover>::Service: tower::Service<I>,
    <<M::Response as Discover>::Service as tower::Service<I>>::Error: Into<Error>,
    Balance<PendingRequestsDiscover<M::Response>, I>: tower::Service<I>,
{
    type Response = Balance<PendingRequestsDiscover<M::Response>, I>;
    type Error = M::Error;
    type Future = future::Map<M::Future, fn(M::Response) -> Self::Response>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.inner.poll_ready()
    }

    fn call(&mut self, target: T) -> Self::Future {
        self.inner.call(target).map(|d| {
            Balance::new(
                PendingRequestsDiscover::new(d, NoInstrument),
                SmallRng::from_entropy(),
            )
        })
    }
}

impl<M: Clone, I> Clone for MakeSvc<M, I> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            _marker: self._marker,
        }
    }
}
