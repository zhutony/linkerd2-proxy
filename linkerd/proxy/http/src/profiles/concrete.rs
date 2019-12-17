use super::{WeightedAddr, WithAddr};
use futures::{future, Async, Future, Poll};
use indexmap::IndexMap;
use linkerd2_addr::NameAddr;
use linkerd2_error::Error;
use linkerd2_stack::Make;
use rand::distributions::{Distribution, WeightedIndex};
use rand::rngs::SmallRng;
use tokio::sync::watch;
pub use tokio::sync::watch::error::SendError;
use tracing::{debug, trace};

pub fn forward<T, M>(target: T, make: M, rng: SmallRng) -> (Service<M::Service>, Update<T, M>)
where
    T: Clone + WithAddr,
    M: Make<T>,
    M::Service: Clone,
{
    let routes = Routes::Forward {
        addr: None,
        service: make.make(target.clone()),
    };
    let (tx, rx) = watch::channel(routes.clone());
    let concrete = Service {
        routes: routes.clone(),
        updates: rx.clone(),
        next_split_index: None,
        rng,
    };
    let update = Update {
        target,
        make,
        tx,
        routes,
    };
    (concrete, update)
}

#[derive(Clone, Debug)]
pub struct Service<S> {
    routes: Routes<S>,
    updates: watch::Receiver<Routes<S>>,
    next_split_index: Option<usize>,
    rng: SmallRng,
}

#[derive(Debug)]
pub struct Update<T, M: Make<T>> {
    target: T,
    make: M,
    routes: Routes<M::Service>,
    tx: watch::Sender<Routes<M::Service>>,
}

#[derive(Clone)]
enum Routes<S> {
    Forward {
        addr: Option<NameAddr>,
        service: S,
    },
    Split {
        distribution: WeightedIndex<u32>,
        services: IndexMap<NameAddr, S>,
    },
}

impl<S> std::fmt::Debug for Routes<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Routes::Forward { .. } => write!(f, "Routes::Forward(..)"),
            Routes::Split { services, .. } => {
                write!(f, "Routes::Split(")?;
                let mut addrs = services.keys();
                if let Some(a) = addrs.next() {
                    write!(f, "{}", a)?;
                }
                for a in addrs {
                    write!(f, ", {}", a)?;
                }
                write!(f, ")")
            }
        }
    }
}

impl<Req, S> tower::Service<Req> for Service<S>
where
    S: tower::Service<Req> + Clone,
    S::Error: Into<Error>,
{
    type Response = S::Response;
    type Error = Error;
    type Future = future::MapErr<S::Future, fn(S::Error) -> Error>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        loop {
            match self.updates.poll_ref().map_err(Error::from)? {
                Async::NotReady | Async::Ready(None) => break,
                Async::Ready(Some(routes)) => {
                    debug!(?routes, "updated");
                    self.next_split_index = None;
                    self.routes = (*routes).clone();
                }
            }
        }

        match self.routes {
            Routes::Forward {
                ref mut service, ..
            } => service.poll_ready().map_err(Into::into),

            Routes::Split {
                ref distribution,
                ref mut services,
            } => {
                debug_assert!(services.len() > 1);
                if self.next_split_index.is_some() {
                    return Ok(Async::Ready(()));
                }

                // Note: this may not poll all inner services, but at least
                // polls _some_ inner services.
                for _ in 0..services.len() {
                    let idx = distribution.sample(&mut self.rng);
                    let (_, svc) = services
                        .get_index_mut(idx)
                        .expect("split index out of range");
                    if svc.poll_ready().map_err(Into::into)?.is_ready() {
                        self.next_split_index = Some(idx);
                        return Ok(Async::Ready(()));
                    }
                }

                // We at least did some polling.
                debug!(attempts = services.len(), "not ready");
                Ok(Async::NotReady)
            }
        }
    }

    fn call(&mut self, req: Req) -> Self::Future {
        match self.routes {
            Routes::Forward {
                ref mut service, ..
            } => service.call(req).map_err(Into::into),

            Routes::Split {
                ref mut services, ..
            } => {
                let idx = self
                    .next_split_index
                    .take()
                    .expect("concrete router is not ready");
                let (_, svc) = services
                    .get_index_mut(idx)
                    .expect("split index out of range");
                svc.call(req).map_err(Into::into)
            }
        }
    }
}

impl<T, M> Update<T, M>
where
    T: Clone,
    M: Make<T>,
    M::Service: Clone,
{
    pub fn set_forward(&mut self) -> Result<(), error::LostService> {
        if let Routes::Forward { addr: None, .. } = self.routes {
            trace!("default forward already set");
            return Ok(());
        };

        trace!("building default forward");
        self.routes = Routes::Forward {
            service: self.make.make(self.target.clone()),
            addr: None,
        };

        self.tx
            .broadcast(self.routes.clone())
            .map_err(|_| error::LostService(()))
    }

    pub fn set_split(&mut self, mut addrs: Vec<WeightedAddr>) -> Result<(), error::LostService>
    where
        T: WithAddr,
    {
        let routes = match self.routes {
            Routes::Forward { ref addr, .. } => {
                if addrs.len() == 1 {
                    let new_addr = addrs.pop().unwrap().addr;
                    if addr.as_ref().map(|a| a == &new_addr).unwrap_or(false) {
                        trace!("forward already set to {}", new_addr);
                        return Ok(());
                    }

                    trace!("building forward to {}", new_addr);
                    let service = {
                        let t = self.target.clone().with_addr(new_addr.clone());
                        self.make.make(t)
                    };
                    Routes::Forward {
                        addr: Some(new_addr),
                        service,
                    }
                } else {
                    let distribution = WeightedIndex::new(addrs.iter().map(|w| w.weight))
                        .expect("invalid weight distribution");
                    let services = addrs
                        .into_iter()
                        .map(|wa| {
                            let t = self.target.clone().with_addr(wa.addr.clone());
                            let s = self.make.make(t);
                            (wa.addr, s)
                        })
                        .collect::<IndexMap<_, _>>();
                    Routes::Split {
                        distribution,
                        services,
                    }
                }
            }
            Routes::Split { ref services, .. } => {
                if addrs.len() == 1 {
                    let new_addr = addrs.pop().unwrap().addr;
                    let service = match services.get(&new_addr) {
                        Some(service) => service.clone(),
                        None => {
                            trace!("building forward to {}", new_addr);
                            let t = self.target.clone().with_addr(new_addr.clone());
                            self.make.make(t)
                        }
                    };
                    Routes::Forward {
                        addr: Some(new_addr),
                        service,
                    }
                } else {
                    let distribution = WeightedIndex::new(addrs.iter().map(|w| w.weight))
                        .expect("invalid weight distribution");
                    let prior = services;
                    let mut services = IndexMap::with_capacity(addrs.len());
                    trace!("building split over {} services", addrs.len());
                    for w in addrs.into_iter() {
                        match prior.get(&w.addr) {
                            None => {
                                trace!("building split to {}", w.addr);
                                let t = self.target.clone().with_addr(w.addr.clone());
                                let s = self.make.make(t);
                                services.insert(w.addr, s);
                            }
                            Some(s) => {
                                trace!("reusing split to {}", w.addr);
                                services.insert(w.addr, (*s).clone());
                            }
                        }
                    }
                    Routes::Split {
                        distribution,
                        services,
                    }
                }
            }
        };

        self.routes = routes.clone();
        self.tx
            .broadcast(routes)
            .map_err(|_| error::LostService(()))
    }
}

pub mod error {
    #[derive(Debug)]
    pub struct LostService(pub(super) ());

    impl std::fmt::Display for LostService {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "services lost")
        }
    }

    impl std::error::Error for LostService {}
}
