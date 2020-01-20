use http;
use indexmap::IndexMap;
use linkerd2_metrics::{latency, Counter, FmtLabels, Histogram};
use std::hash::Hash;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio_timer::clock;

pub mod classify;
pub mod handle_time;
mod report;
mod service;

pub use self::{report::Report, service::Layer};

pub fn new<T, C>(retain_idle: Duration) -> (Requests<T, C>, Report<T, RequestMetrics<C>>)
where
    T: FmtLabels + Clone + Hash + Eq,
    C: FmtLabels + Hash + Eq,
{
    let registry = Arc::new(Mutex::new(Registry::default()));
    let report = Report::new(retain_idle, registry);
    (Requests(registry.clone()), report)
}

#[derive(Debug)]
pub struct Requests<T, C>(Arc<Mutex<Registry<T, RequestMetrics<C>>>>)
where
    T: Hash + Eq,
    C: Hash + Eq;

#[derive(Debug)]
pub struct Retries<T>(Arc<Mutex<Registry<T, RetryMetrics>>>)
where
    T: Hash + Eq;

impl<T: Hash + Eq, C: Hash + Eq> Clone for Requests<T, C> {
    fn clone(&self) -> Self {
        Requests(self.0.clone())
    }
}

impl<T: Hash + Eq> Clone for Retries<T> {
    fn clone(&self) -> Self {
        Retries(self.0.clone())
    }
}

#[derive(Debug)]
struct Registry<T, M>
where
    T: Hash + Eq,
{
    by_target: IndexMap<T, Arc<Mutex<M>>>,
}

trait LastUpdate {
    fn last_update(&self) -> Instant;
}

#[derive(Debug)]
pub struct RequestMetrics<C>
where
    C: Hash + Eq,
{
    last_update: Instant,
    total: Counter,
    by_status: IndexMap<Option<http::StatusCode>, StatusMetrics<C>>,
}

#[derive(Debug)]
pub struct RetryMetrics {
    last_update: Instant,
    skipped: Counter,
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

// #[derive(Debug, PartialEq, Eq, Hash)]
// enum RetrySkipped {
//     Budget,
// }

impl<T, M> Default for Registry<T, M>
where
    T: Hash + Eq,
{
    fn default() -> Self {
        Self {
            by_target: IndexMap::default(),
        }
    }
}

impl<T, M> Registry<T, M>
where
    T: Hash + Eq,
    M: LastUpdate,
{
    /// Retains metrics for all targets that (1) no longer have an active
    /// reference to the `RequestMetrics` structure and (2) have not been updated since `epoch`.
    fn retain_since(&mut self, epoch: Instant) {
        self.by_target.retain(|_, m| {
            Arc::strong_count(&m) > 1 || m.lock().map(|m| m.last_update() >= epoch).unwrap_or(false)
        })
    }
}

impl<C> Default for RequestMetrics<C>
where
    C: Hash + Eq,
{
    fn default() -> Self {
        Self {
            last_update: clock::now(),
            total: Counter::default(),
            by_status: IndexMap::default(),
        }
    }
}

impl<C> LastUpdate for RequestMetrics<C>
where
    C: Hash + Eq,
{
    fn last_update(&self) -> Instant {
        self.last_update
    }
}

impl Default for RetryMetrics {
    fn default() -> Self {
        Self {
            last_update: clock::now(),
            skipped: Counter::default(),
        }
    }
}

impl LastUpdate for RetryMetrics {
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

#[cfg(test)]
mod tests {
    #[test]
    fn expiry() {
        use crate::metrics::FmtLabels;
        use std::fmt;
        use std::time::Duration;
        use tokio_timer::clock;

        #[derive(Clone, Debug, Hash, Eq, PartialEq)]
        struct Target(usize);
        impl FmtLabels for Target {
            fn fmt_labels(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "n=\"{}\"", self.0)
            }
        }

        #[allow(dead_code)]
        #[derive(Clone, Debug, Hash, Eq, PartialEq)]
        enum Class {
            Good,
            Bad,
        };
        impl FmtLabels for Class {
            fn fmt_labels(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                use std::fmt::Display;
                match self {
                    Class::Good => "class=\"good\"".fmt(f),
                    Class::Bad => "class=\"bad\"".fmt(f),
                }
            }
        }

        let retain_idle_for = Duration::from_secs(1);
        let (r, report) = super::new::<Target, Class>(retain_idle_for);
        let mut registry = r.lock().unwrap();

        let before_update = clock::now();
        let metrics = registry
            .by_target
            .entry(Target(123))
            .or_insert_with(|| Default::default())
            .clone();
        assert_eq!(registry.by_target.len(), 1, "target should be registered");
        let after_update = clock::now();

        registry.retain_since(after_update);
        assert_eq!(
            registry.by_target.len(),
            1,
            "target should not be evicted by time alone"
        );

        drop(metrics);
        registry.retain_since(before_update);
        assert_eq!(
            registry.by_target.len(),
            1,
            "target should not be evicted by availability alone"
        );

        registry.retain_since(after_update);
        assert_eq!(
            registry.by_target.len(),
            0,
            "target should be evicted by time once the handle is dropped"
        );

        drop((registry, report));
    }
}
