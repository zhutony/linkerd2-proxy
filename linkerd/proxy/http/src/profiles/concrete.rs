use futures::{Async, Poll};
use indexmap::IndexMap;
use linkerd2_addr::NameAddr;
use rand::distributions::{Distribution, WeightedIndex};
use rand::rngs::SmallRng;

#[derive(Clone)]
pub enum Concrete<S> {
    Service(S),
    Split {
        services: IndexMap<NameAddr, S>,
        distribution: WeightedIndex<u32>,
        ready_index: Option<usize>,
        rng: SmallRng,
        max_tries: usize,
    },
}

impl<S> Concrete<S> {
    pub fn service(service: S) -> Self {
        Concrete::Service(service)
    }

    pub fn split(services: IndexMap<NameAddr, (u32, S)>, rng: SmallRng) -> Self {
        assert!(services.len() > 0);
        let distribution = WeightedIndex::new(services.values().map(|(w, _)| w))
            .expect("invalid weight distribution");

        let services = services
            .into_iter()
            .map(|(a, (_, s))| (a, s).into())
            .collect::<IndexMap<_, _>>();

        Concrete::Split {
            max_tries: services.len(),
            services,
            distribution,
            ready_index: None,
            rng,
        }
    }

    pub fn with_max_tries(mut self, max_tries: usize) -> Self {
        if let Concrete::Split {
            ref mut max_tries,
            ref services,
            ..
        } = self
        {
            *max_tries = max_tries.min(services.len());
        }
        self
    }
}

impl<Req, S> tower::Service<Req> for Concrete<S>
where
    S: tower::Service<Req>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        if self.ready_index.is_some() {
            return Ok(Async::Ready(()));
        }

        for _ in 0..self.max_tries {
            let idx = self.distribution.sample(&mut self.rng);
            match self.services.get_index_mut(idx).unwrap().1.poll_ready() {
                Ok(Async::NotReady) => {}
                ret => return ret,
            }
        }

        // We at least did some polling.
        Ok(Async::NotReady)
    }

    fn call(&mut self, req: Req) -> Self::Future {
        let idx = self.ready_index.take().expect("poll_ready must be called");
        self.services.get_index_mut(idx).unwrap().1.call(req)
    }
}
