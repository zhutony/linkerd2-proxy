use crate::per_service;
pub use tower::layer::Layer;

/// Make a `Layer` from a closure.
pub fn mk<F>(f: F) -> LayerFn<F> {
    LayerFn(f)
}

#[derive(Clone, Copy, Debug)]
pub struct LayerFn<F>(F);

impl<F, S, Out> Layer<S> for LayerFn<F>
where
    F: Fn(S) -> Out,
{
    type Service = Out;

    fn layer(&self, inner: S) -> Self::Service {
        (self.0)(inner)
    }
}

/// Extending `impl Layer`s with useful methods.
pub trait LayerExt<S>: Layer<S> {
    /// Apply this layer to a `MakeService` such that every made service
    /// has this layer applied.
    fn per_service(self) -> per_service::Layer<Self>
    where
        Self: Clone + Sized,
    {
        per_service::layer(self)
    }
}

impl<L, S> LayerExt<S> for L where L: Layer<S> {}
