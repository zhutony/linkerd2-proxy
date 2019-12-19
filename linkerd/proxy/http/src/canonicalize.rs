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

use futures::{try_ready, Async, Future, Poll};
use linkerd2_addr::{Addr, NameAddr};
use linkerd2_dns as dns;
use std::time::Duration;
use tokio::timer::Timeout;
use tracing::warn;

// FIXME the resolver should be abstracted to a trait so that this can be tested
// without a real DNS service.
#[derive(Debug, Clone)]
pub struct Layer {
    resolver: dns::Resolver,
    timeout: Duration,
}

#[derive(Clone, Debug)]
pub struct Stack<M> {
    resolver: dns::Resolver,
    inner: M,
    timeout: Duration,
}

pub enum MakeFuture<M: tower::Service<Addr>> {
    Refine {
        future: Timeout<dns::RefineFuture>,
        original: NameAddr,
        make: M,
    },
    Ready(M, Addr),
    Make(M::Future),
}

// === Layer ===

impl Layer {
    pub fn new(resolver: dns::Resolver, timeout: Duration) -> Self {
        Layer { resolver, timeout }
    }
}

impl<M> tower::layer::Layer<M> for Layer
where
    M: tower::Service<Addr> + Clone,
{
    type Service = Stack<M>;

    fn layer(&self, inner: M) -> Self::Service {
        Stack {
            inner,
            resolver: self.resolver.clone(),
            timeout: self.timeout,
        }
    }
}

// === impl Stack ===

impl<M> tower::Service<Addr> for Stack<M>
where
    M: tower::Service<Addr> + Clone,
{
    type Response = M::Response;
    type Error = M::Error;
    type Future = MakeFuture<M>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.inner.poll_ready()
    }

    fn call(&mut self, addr: Addr) -> Self::Future {
        match addr.clone() {
            Addr::Socket(_) => MakeFuture::Ready(self.inner.clone(), addr),
            Addr::Name(original) => {
                let refine = self.resolver.refine(original.name());
                MakeFuture::Refine {
                    future: Timeout::new(refine, self.timeout),
                    make: self.inner.clone(),
                    original,
                }
            }
        }
    }
}

// === impl MakeFuture ===

impl<M> Future for MakeFuture<M>
where
    M: tower::Service<Addr> + Clone,
{
    type Item = M::Response;
    type Error = M::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        loop {
            *self = match self {
                MakeFuture::Refine {
                    ref mut future,
                    original,
                    make,
                } => match future.poll() {
                    Ok(Async::NotReady) => return Ok(Async::NotReady),
                    Ok(Async::Ready(refine)) => {
                        let name = NameAddr::new(refine.name, original.port());
                        MakeFuture::Ready(make.clone(), name.into())
                    }
                    Err(error) => {
                        warn!(%error, name = %original, "failed to refine name via DNS");
                        MakeFuture::Ready(make.clone(), original.clone().into())
                    }
                },
                MakeFuture::Ready(make, addr) => {
                    try_ready!(make.poll_ready());
                    MakeFuture::Make(make.call(addr.clone()))
                }
                MakeFuture::Make(ref mut fut) => return fut.poll(),
            };
        }
    }
}
