use crate::Router;
use futures::Future;
use linkerd2_stack::Make;
use std::time::Duration;
use tracing::info_span;
use tracing_futures::Instrument;

#[derive(Clone, Debug)]
struct Config {
    capacity: usize,
    max_idle_age: Duration,
}

/// A layer that that builds a routing service.
///
/// A `Rec`-typed `Recognize` instance is used to produce a target for each
/// `Req`-typed request. If the router doesn't already have a `Service` for this
/// target, it uses a `Mk`-typed `Service` stack.
#[derive(Clone, Debug)]
pub struct Layer {
    config: Config,
}

#[derive(Clone, Debug)]
pub struct MakeRouter<M> {
    config: Config,
    inner: M,
}

// === impl Config ===

// === impl Layer ===

impl Layer {
    pub fn new(capacity: usize, max_idle_age: Duration) -> Self {
        Self {
            config: Config {
                capacity,
                max_idle_age,
            },
        }
    }
}

impl<M> tower::layer::Layer<M> for Layer {
    type Service = MakeRouter<M>;

    fn layer(&self, inner: M) -> Self::Service {
        MakeRouter {
            inner,
            config: self.config.clone(),
        }
    }
}

// === impl MakeRouter ===

impl<M> MakeRouter<M> {
    pub fn spawn<T>(&self) -> Router<T, M>
    where
        T: Clone + Eq + std::hash::Hash + Send + 'static,
        M: Make<T> + Clone + Send + Sync + 'static,
        M::Service: Clone + Send + Sync + 'static,
    {
        let (router, purge) = Router::new(
            self.inner.clone(),
            self.config.capacity,
            self.config.max_idle_age,
        );
        tokio::spawn(
            purge
                .map_err(|e| match e {})
                .instrument(info_span!("router.purge")),
        );
        router
    }
}
