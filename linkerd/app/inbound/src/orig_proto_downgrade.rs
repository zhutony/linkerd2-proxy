use linkerd2_app_core::{proxy::http::orig_proto, svc};
use std::marker::PhantomData;
use tracing::trace;

#[derive(Debug)]
pub struct Layer<A, B>(PhantomData<fn(A) -> B>);

#[derive(Debug)]
pub struct Stack<M, A, B> {
    inner: M,
    _marker: PhantomData<fn(A) -> B>,
}

// === impl Layer ===

pub fn layer<A, B>() -> Layer<A, B> {
    Layer(PhantomData)
}

impl<A, B> Clone for Layer<A, B> {
    fn clone(&self) -> Self {
        Layer(PhantomData)
    }
}

impl<M, A, B> tower::layer::Layer<M> for Layer<A, B> {
    type Service = Stack<M, A, B>;

    fn layer(&self, inner: M) -> Self::Service {
        Stack {
            inner,
            _marker: self.0,
        }
    }
}

// === impl Stack ===

impl<M: Clone, A, B> Clone for Stack<M, A, B> {
    fn clone(&self) -> Self {
        Stack {
            inner: self.inner.clone(),
            _marker: PhantomData,
        }
    }
}

impl<T, M, A, B> svc::Make<T> for Stack<M, A, B>
where
    M: svc::Make<T>,
    M::Service: svc::Service<http::Request<A>, Response = http::Response<B>>,
{
    type Service = orig_proto::Downgrade<M::Service>;

    fn make(&self, target: T) -> Self::Service {
        trace!("supporting {} downgrades", orig_proto::L5D_ORIG_PROTO);
        orig_proto::Downgrade::new(self.inner.make(target))
    }
}
