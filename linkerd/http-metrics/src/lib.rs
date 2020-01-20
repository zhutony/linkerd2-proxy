pub use self::{requests::Requests, retries::Retries};
use indexmap::IndexMap;
use std::fmt;
use std::hash::Hash;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub mod requests;
pub mod retries;

#[derive(Debug)]
struct Registry<T, M>
where
    T: Hash + Eq,
{
    by_target: IndexMap<T, Arc<Mutex<M>>>,
}

/// Reports metrics for prometheus.
#[derive(Clone, Debug)]
pub struct Report<T, M>
where
    T: Hash + Eq,
{
    prefix: &'static str,
    registry: Arc<Mutex<Registry<T, M>>>,
    retain_idle: Duration,
}

struct Prefixed<'p, N: fmt::Display> {
    prefix: &'p str,
    name: N,
}

trait LastUpdate {
    fn last_update(&self) -> Instant;
}

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

impl<T, M> Report<T, M>
where
    T: Hash + Eq,
{
    fn new(retain_idle: Duration, registry: Arc<Mutex<Registry<T, M>>>) -> Self {
        Self {
            prefix: "",
            registry,
            retain_idle,
        }
    }

    pub fn with_prefix(self, prefix: &'static str) -> Self {
        if prefix.is_empty() {
            return self;
        }

        Self { prefix, ..self }
    }

    fn prefix_key<N: fmt::Display>(&self, name: N) -> Prefixed<'_, N> {
        Prefixed {
            prefix: &self.prefix,
            name,
        }
    }
}

impl<'p, N: fmt::Display> fmt::Display for Prefixed<'p, N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.prefix.is_empty() {
            return self.name.fmt(f);
        }

        write!(f, "{}_{}", self.prefix, self.name)
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
