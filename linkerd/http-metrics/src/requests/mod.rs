use super::{LastUpdate, Registry, Report};
use http;
use indexmap::IndexMap;
use linkerd2_http_classify::ClassifyResponse;
use linkerd2_metrics::{latency, Counter, Histogram};
use std::fmt::Debug;
use std::hash::Hash;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio_timer::clock;

mod layer;

type SharedRegistry<T, C> = Arc<Mutex<Registry<T, Metrics<C>>>>;

#[derive(Debug)]
pub struct Requests<T, C>(SharedRegistry<T, C>)
where
    T: Hash + Eq,
    C: Hash + Eq;

#[derive(Debug)]
pub struct Metrics<C>
where
    C: Hash + Eq,
{
    last_update: Instant,
    total: Counter,
    by_status: IndexMap<Option<http::StatusCode>, StatusMetrics<C>>,
}

#[derive(Debug)]
struct StatusMetrics<C>
where
    C: Hash + Eq,
{
    latency: Histogram<latency::Ms>,
    by_class: IndexMap<C, ClassMetrics>,
}

#[derive(Debug, Default)]
pub struct ClassMetrics {
    total: Counter,
}

// === impl Requests ===

impl<T: Hash + Eq, C: Hash + Eq> Default for Requests<T, C> {
    fn default() -> Self {
        Requests(Arc::new(Mutex::new(Registry::default())))
    }
}

impl<T: Hash + Eq, C: Hash + Eq> Requests<T, C> {
    pub fn into_report(self, retain_idle: Duration) -> Report<T, Metrics<C>> {
        Report::new(retain_idle, self.0)
    }

    pub fn into_layer<L>(self) -> layer::Layer<T, L>
    where
        L: ClassifyResponse<Class = C> + Send + Sync + 'static,
    {
        layer::Layer::new(self.0)
    }
}

impl<T: Hash + Eq, C: Hash + Eq> Clone for Requests<T, C> {
    fn clone(&self) -> Self {
        Requests(self.0.clone())
    }
}

// === impl Metrics ===

impl<C: Hash + Eq> Default for Metrics<C> {
    fn default() -> Self {
        Self {
            last_update: clock::now(),
            total: Counter::default(),
            by_status: IndexMap::default(),
        }
    }
}

impl<C: Hash + Eq> LastUpdate for Metrics<C> {
    fn last_update(&self) -> Instant {
        self.last_update
    }
}

impl<C> Default for StatusMetrics<C>
where
    C: Hash + Eq,
{
    fn default() -> Self {
        Self {
            latency: Histogram::default(),
            by_class: IndexMap::default(),
        }
    }
}
