//! A stack module that lazily, dynamically resolves an `Addr` target, via DNS,
//! to determine it's canonical fully qualified domain name.
//!
//! For example, an application may set an authority value like `web:8080` with a
//! resolv.conf(5) search path of `example.com example.net`. In such a case,
//! this module may build its inner stack with either `web.example.com.:8080`,
//! `web.example.net.:8080`, or `web:8080`, depending on the state of DNS.
//!
//! DNS TTLs are honored and the most recent value is added to each request's
//! extensions.

use futures::{Async, Future, Poll};
use linkerd2_addr::{Addr, NameAddr};
use linkerd2_dns as dns;
use std::time::Duration;
use tokio::timer::Timeout;
use tower::util::{Oneshot, ServiceExt};
use tracing::warn;

pub trait Target {
    fn addr(&self) -> &Addr;
    fn addr_mut(&mut self) -> &mut Addr;
}

// FIXME the resolver should be abstracted to a trait so that this can be tested
// without a real DNS service.
#[derive(Debug, Clone)]
pub struct Layer {
    resolver: dns::Resolver,
    timeout: Duration,
}

#[derive(Clone, Debug)]
pub struct Canonicalize<M> {
    inner: M,
    timeout: Duration,
    resolver: dns::Resolver,
}

pub enum MakeFuture<T, M: tower::Service<T>> {
    Refine {
        future: Timeout<dns::RefineFuture>,
        make: Option<M>,
        original: Option<T>,
    },
    Make(Oneshot<M, T>),
}

// === Layer ===

impl Layer {
    pub fn new(resolver: dns::Resolver, timeout: Duration) -> Self {
        Layer { resolver, timeout }
    }
}

impl<M> tower::layer::Layer<M> for Layer {
    type Service = Canonicalize<M>;

    fn layer(&self, inner: M) -> Self::Service {
        Self::Service {
            inner,
            timeout: self.timeout,
            resolver: self.resolver.clone(),
        }
    }
}

// === impl Canonicalize ===

impl<T, M> tower::Service<T> for Canonicalize<M>
where
    T: Target + Clone,
    M: tower::Service<T> + Clone,
{
    type Response = M::Response;
    type Error = M::Error;
    type Future = MakeFuture<T, M>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        Ok(().into())
    }

    fn call(&mut self, target: T) -> Self::Future {
        match target.addr().name_addr() {
            None => MakeFuture::Make(self.inner.clone().oneshot(target)),
            Some(na) => {
                let refine = self.resolver.refine(na.name());
                MakeFuture::Refine {
                    original: Some(target.clone()),
                    make: Some(self.inner.clone()),
                    future: Timeout::new(refine, self.timeout),
                }
            }
        }
    }
}

// === impl MakeFuture ===

impl<T, M> Future for MakeFuture<T, M>
where
    T: Target,
    M: tower::Service<T> + Clone,
{
    type Item = M::Response;
    type Error = M::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        loop {
            *self = match self {
                MakeFuture::Make(ref mut fut) => return fut.poll(),
                MakeFuture::Refine {
                    ref mut future,
                    ref mut make,
                    ref mut original,
                } => match future.poll() {
                    Ok(Async::NotReady) => return Ok(Async::NotReady),
                    Ok(Async::Ready(refine)) => {
                        let make = make.take().expect("illegal state");
                        let mut target = original.take().expect("illegal state");
                        let name = NameAddr::new(refine.name, target.addr().port());
                        *target.addr_mut() = name.into();
                        MakeFuture::Make(make.oneshot(target))
                    }
                    Err(error) => {
                        let make = make.take().expect("illegal state");
                        let target = original.take().expect("illegal state");
                        warn!(%error, addr = %target.addr(), "failed to refine name via DNS");
                        MakeFuture::Make(make.oneshot(target))
                    }
                },
            };
        }
    }
}
