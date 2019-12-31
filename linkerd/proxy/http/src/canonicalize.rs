//! A stack module that lazily, dynamically resolves an `Addr` target, via DNS,
//! to determine it's canonical fully qualified domain name.
//!
//! For example, an application may set an authority value like `web:8080` with a
//! resolv.conf(5) search path of `example.com example.net`. In such a case,
//! this module may build its inner stack with either `web.example.com.:8080`,
//! `web.example.net.:8080`, or `web:8080`, depending on the state of DNS.

use futures::{try_ready, Async, Future, Poll};
use linkerd2_addr::{Addr, NameAddr};
use linkerd2_dns::{Name, Refine};
use linkerd2_error::Error;
use std::time::Duration;
use tokio::timer::{timeout, Timeout};
use tracing::warn;

pub trait Target {
    fn addr(&self) -> &Addr;
    fn addr_mut(&mut self) -> &mut Addr;
}

// FIXME the resolver should be abstracted to a trait so that this can be tested
// without a real DNS service.
#[derive(Debug, Clone)]
pub struct Layer<R> {
    resolver: R,
    timeout: Duration,
}

#[derive(Clone, Debug)]
pub struct Canonicalize<R, M> {
    inner: M,
    resolver: R,
    timeout: Duration,
}

pub enum MakeFuture<T, R, M: tower::Service<T>> {
    Refine {
        future: Timeout<R>,
        make: Option<M>,
        original: Option<T>,
    },
    NotReady(M, Option<T>),
    Make(M::Future),
}

// === Layer ===

impl<R> Layer<R> {
    pub fn new(resolver: R, timeout: Duration) -> Self {
        Layer { resolver, timeout }
    }
}

impl<R: Clone, M> tower::layer::Layer<M> for Layer<R> {
    type Service = Canonicalize<R, M>;

    fn layer(&self, inner: M) -> Self::Service {
        Self::Service {
            inner,
            timeout: self.timeout,
            resolver: self.resolver.clone(),
        }
    }
}

// === impl Canonicalize ===

impl<T, R, M> tower::Service<T> for Canonicalize<R, M>
where
    T: Target + Clone,
    R: tower::Service<Name, Response = Refine> + Clone,
    R::Error: Into<Error>,
    timeout::Error<R::Error>: Into<Error>,
    M: tower::Service<T> + Clone,
    M::Error: Into<Error>,
{
    type Response = M::Response;
    type Error = Error;
    type Future = MakeFuture<T, R::Future, M>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        try_ready!(self.resolver.poll_ready().map_err(Into::into));
        try_ready!(self.inner.poll_ready().map_err(Into::into));
        Ok(().into())
    }

    fn call(&mut self, target: T) -> Self::Future {
        match target.addr().name_addr() {
            None => {
                self.resolver = self.resolver.clone();
                MakeFuture::Make(self.inner.call(target))
            }
            Some(na) => {
                let refine = self.resolver.call(na.name().clone());

                let inner = self.inner.clone();
                let make = std::mem::replace(&mut self.inner, inner);

                MakeFuture::Refine {
                    make: Some(make),
                    original: Some(target.clone()),
                    future: Timeout::new(refine, self.timeout),
                }
            }
        }
    }
}

// === impl MakeFuture ===

impl<T, R, M> Future for MakeFuture<T, R, M>
where
    T: Target,
    R: Future<Item = Refine>,
    timeout::Error<R::Error>: Into<Error>,
    M: tower::Service<T> + Clone,
    M::Error: Into<Error>,
{
    type Item = M::Response;
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        loop {
            *self = match self {
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
                        MakeFuture::NotReady(make, Some(target))
                    }
                    Err(error) => {
                        let make = make.take().expect("illegal state");
                        let target = original.take().expect("illegal state");
                        let error: Error = error.into();
                        warn!(%error, addr = %target.addr(), "failed to refine name via DNS");
                        MakeFuture::NotReady(make, Some(target))
                    }
                },
                MakeFuture::NotReady(ref mut svc, ref mut target) => {
                    try_ready!(svc.poll_ready().map_err(Into::into));
                    let target = target.take().expect("illegal state");
                    MakeFuture::Make(svc.call(target))
                }
                MakeFuture::Make(ref mut fut) => return fut.poll().map_err(Into::into),
            };
        }
    }
}
