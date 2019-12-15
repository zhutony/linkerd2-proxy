use super::{WeightedAddr, WithAddr};
use futures::{Async, Poll};
use indexmap::IndexMap;
use linkerd2_addr::NameAddr;
use linkerd2_stack::Make;
use rand::distributions::{Distribution, WeightedIndex};
use rand::rngs::SmallRng;

pub struct Concrete<T, M: Make<T>> {
    target: T,
    make: M,
    rng: SmallRng,
    inner: Inner<M::Service>,
}

enum Inner<S> {
    Forward(S),
    Split {
        distribution: WeightedIndex<u32>,
        services: IndexMap<NameAddr, S>,
        ready_index: Option<usize>,
    },
}

impl<T: Clone, M: Make<T>> Concrete<T, M> {
    pub fn forward(target: T, make: M, rng: SmallRng) -> Self {
        let inner = Inner::Forward(make.make(target.clone()));
        Self {
            target,
            make,
            rng,
            inner,
        }
    }

    pub fn set_forward(&mut self) {
        if let Inner::Split { .. } = self.inner {
            self.inner = Inner::Forward(self.make.make(self.target.clone()));
        }
    }

    pub fn set_split(&mut self, addrs: Vec<WeightedAddr>)
    where
        T: WithAddr,
    {
        self.inner = match self.inner {
            Inner::Forward { .. } => {
                if addrs.len() == 1 {
                    Inner::Forward(self.make.make(self.target.clone()))
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
                    Inner::Split {
                        distribution,
                        services,
                        ready_index: None,
                    }
                }
            }
            Inner::Split {
                ref mut services, ..
            } => {
                let distribution = WeightedIndex::new(addrs.iter().map(|w| w.weight))
                    .expect("invalid weight distribution");
                let prior = services;
                let mut services = IndexMap::with_capacity(addrs.len());
                for w in addrs.into_iter() {
                    match prior.swap_remove(&w.addr) {
                        Some(s) => {
                            services.insert(w.addr, s);
                        }
                        None => {
                            let t = self.target.clone().with_addr(w.addr.clone());
                            let s = self.make.make(t);
                            services.insert(w.addr, s);
                        }
                    }
                }
                Inner::Split {
                    distribution,
                    services,
                    ready_index: None,
                }
            }
        };
    }
}

impl<Req, T, M, S> tower::Service<Req> for Concrete<T, M>
where
    M: Make<T, Service = S>,
    S: tower::Service<Req>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        match self.inner {
            Inner::Forward(ref mut svc) => svc.poll_ready(),
            Inner::Split {
                ref distribution,
                ref mut services,
                ref mut ready_index,
            } => {
                // Note: this may not poll all inner services, but at least
                // polls _some_ inner services.
                debug_assert!(services.len() > 1);
                for _ in 0..services.len() {
                    let idx = distribution.sample(&mut self.rng);
                    let (_, svc) = services
                        .get_index_mut(idx)
                        .expect("split index out of range");
                    match svc.poll_ready() {
                        Ok(Async::NotReady) => {}
                        Err(e) => return Err(e),
                        Ok(Async::Ready(())) => {
                            *ready_index = Some(idx);
                            return Ok(Async::Ready(()));
                        }
                    }
                }

                // We at least did some polling.
                Ok(Async::NotReady)
            }
        }
    }

    fn call(&mut self, req: Req) -> Self::Future {
        match self.inner {
            Inner::Forward(ref mut svc) => svc.call(req),
            Inner::Split {
                ref mut services,
                ref mut ready_index,
                ..
            } => {
                let idx = ready_index.take().expect("poll_ready must be called");
                let (_, svc) = services
                    .get_index_mut(idx)
                    .expect("ready index out of range");
                svc.call(req)
            }
        }
    }
}
