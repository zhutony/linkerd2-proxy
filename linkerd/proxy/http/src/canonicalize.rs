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
use http;
use linkerd2_addr::{Addr, NameAddr};
use linkerd2_dns as dns;
use linkerd2_error::Error;
use linkerd2_stack::Make;
use std::time::Duration;
use tokio;
use tokio::sync::watch;
use tokio_timer::{clock, Delay, Timeout};
use tower::util::{Oneshot, ServiceExt};
use tracing::{debug, trace, warn};
use tracing_futures::Instrument;

/// Duration to wait before polling DNS again after an error (or a NXDOMAIN
/// response with no TTL).
const DNS_ERROR_TTL: Duration = Duration::from_secs(3);

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

pub struct MakeFuture<F> {
    inner: F,
    task: Option<(NameAddr, dns::Resolver, Duration)>,
}

pub struct Service<S> {
    inner: S,
    rx: watch::Receiver<Option<NameAddr>>,
}

struct Task {
    original: NameAddr,
    resolved: Cache,
    resolver: dns::Resolver,
    state: State,
    timeout: Duration,
    tx: watch::Sender<Option<NameAddr>>,
}

/// Tracks the state of the last resolution.
#[derive(Debug, Clone, Eq, PartialEq)]
enum Cache {
    /// The service has not yet been notified of a value.
    AwaitingInitial,

    /// The service has been notified with the original value (i.e. due to an
    /// error), and we do not yet have a resolved name.
    Unresolved,

    /// The service was last-notified with this name.
    Resolved(NameAddr),
}

enum State {
    Init,
    Pending(Timeout<dns::RefineFuture>),
    ValidUntil(Delay),
}

// === Layer ===

// FIXME the resolver should be abstracted to a trait so that this can be tested
// without a real DNS service.
pub fn layer(resolver: dns::Resolver, timeout: Duration) -> Layer {
    Layer { resolver, timeout }
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

impl<M> Make<Addr> for Stack<M>
where
    M: Make<Addr>,
{
    type Service = tower::util::Either<Service<M::Service>, M::Service>;

    fn make(&self, addr: Addr) -> Self::Service {
        match addr {
            Addr::Socket(_) => tower::util::Either::B(self.inner.make(addr)),
            Addr::Name(ref na) => {
                let (tx, rx) = watch::channel(None);

                let task = Task::new(na.clone(), self.resolver.clone(), self.timeout, tx);
                tokio::spawn(task.in_current_span());

                tower::util::Either::A(Service {
                    inner: self.inner.make(addr),
                    rx,
                })
            }
        }
    }
}

impl<M> tower::Service<Addr> for Stack<M>
where
    M: tower::Service<Addr>,
{
    type Response = tower::util::Either<Service<M::Response>, M::Response>;
    type Error = M::Error;
    type Future = MakeFuture<M::Future>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.inner.poll_ready()
    }

    fn call(&mut self, addr: Addr) -> Self::Future {
        let task = match addr {
            Addr::Name(ref na) => Some((na.clone(), self.resolver.clone(), self.timeout)),
            Addr::Socket(_) => None,
        };

        let inner = self.inner.call(addr);
        MakeFuture { inner, task }
    }
}

// === impl MakeFuture ===

impl<F> Future for MakeFuture<F>
where
    F: Future,
{
    type Item = tower::util::Either<Service<F::Item>, F::Item>;
    type Error = F::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let inner = try_ready!(self.inner.poll());
        let svc = if let Some((na, resolver, timeout)) = self.task.take() {
            let (tx, rx) = watch::channel(None);

            tokio::spawn(Task::new(na, resolver, timeout, tx).in_current_span());

            tower::util::Either::A(Service { inner, rx })
        } else {
            tower::util::Either::B(inner)
        };

        Ok(svc.into())
    }
}

// === impl Task ===

impl Task {
    fn new(
        original: NameAddr,
        resolver: dns::Resolver,
        timeout: Duration,
        tx: watch::Sender<Option<NameAddr>>,
    ) -> Self {
        Self {
            original,
            resolved: Cache::AwaitingInitial,
            resolver,
            state: State::Init,
            timeout,
            tx,
        }
    }
}

impl Future for Task {
    type Item = ();
    type Error = ();

    fn poll(&mut self) -> Poll<(), ()> {
        loop {
            // If the receiver has been dropped, stop watching for updates.
            match self.tx.poll_close() {
                Ok(Async::NotReady) => {}
                _ => {
                    trace!("task complete; name={:?}", self.original);
                    return Ok(Async::Ready(()));
                }
            }

            self.state = match self.state {
                State::Init => {
                    trace!("task init; name={:?}", self.original);
                    let f = self.resolver.refine(self.original.name());
                    State::Pending(Timeout::new(f, self.timeout))
                }
                State::Pending(ref mut fut) => match fut.poll() {
                    Ok(Async::NotReady) => return Ok(Async::NotReady),
                    Ok(Async::Ready(refine)) => {
                        trace!(
                            "task update; name={:?} refined={:?}",
                            self.original,
                            refine.name
                        );
                        // If the resolved name is a new name, bind a
                        // service with it and set a delay that will notify
                        // when the resolver should be consulted again.
                        let resolved = NameAddr::new(refine.name, self.original.port());
                        if self.resolved.get() != Some(&resolved) {
                            self.tx
                                .broadcast(Some(resolved.clone()))
                                .expect("tx failed despite being ready");
                            self.resolved = Cache::Resolved(resolved);
                        }

                        State::ValidUntil(Delay::new(refine.valid_until))
                    }
                    Err(e) => {
                        trace!("task error; name={:?} err={:?}", self.original, e);

                        if self.resolved == Cache::AwaitingInitial {
                            // The service needs a value, so we need to
                            // publish the original name so it can proceed.
                            warn!(
                                "failed to refine {}: {}; using original name",
                                self.original.name(),
                                e,
                            );
                            self.tx
                                .broadcast(Some(self.original.clone()))
                                .expect("tx failed despite being ready");

                            // There's now no need to re-publish the
                            // original name on subsequent failures.
                            self.resolved = Cache::Unresolved;
                        } else {
                            debug!(
                                "failed to refresh {}: {}; cache={:?}",
                                self.original.name(),
                                e,
                                self.resolved,
                            );
                        }

                        let valid_until = e
                            .into_inner()
                            .and_then(|e| match e.kind() {
                                dns::ResolveErrorKind::NoRecordsFound { valid_until, .. } => {
                                    *valid_until
                                }
                                _ => None,
                            })
                            .unwrap_or_else(|| clock::now() + DNS_ERROR_TTL);

                        State::ValidUntil(Delay::new(valid_until))
                    }
                },

                State::ValidUntil(ref mut f) => {
                    trace!("task idle; name={:?}", self.original);

                    match f.poll().expect("timer must not fail") {
                        Async::NotReady => return Ok(Async::NotReady),
                        Async::Ready(()) => {
                            // The last resolution's TTL expired, so issue a new DNS query.
                            State::Init
                        }
                    }
                }
            };
        }
    }
}

impl Cache {
    fn get(&self) -> Option<&NameAddr> {
        match self {
            Cache::Resolved(ref r) => Some(&r),
            _ => None,
        }
    }
}

// === impl Service ===

impl<S, B> tower::Service<http::Request<B>> for Service<S>
where
    S: tower::Service<http::Request<B>> + Clone,
    S::Error: Into<Error>,
{
    type Response = S::Response;
    type Error = Error;
    type Future = ResponseFuture<S, B>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.inner.poll_ready().map_err(Into::into)
    }

    fn call(&mut self, mut req: http::Request<B>) -> Self::Future {
        if let Some(ref na) = *self.rx.get_ref() {
            req.extensions_mut().insert(Addr::from(na.clone()));
            return ResponseFuture::Inner(self.inner.call(req));
        }

        ResponseFuture::Canonicalize {
            inner: Some((req, self.inner.clone())),
            rx: self.rx.clone(),
        }
    }
}

pub enum ResponseFuture<S: tower::Service<http::Request<B>>, B> {
    Inner(S::Future),
    Canonicalize {
        inner: Option<(http::Request<B>, S)>,
        rx: watch::Receiver<Option<NameAddr>>,
    },
    Dispatch(Oneshot<S, http::Request<B>>),
}

impl<S, B> Future for ResponseFuture<S, B>
where
    S: tower::Service<http::Request<B>>,
    S::Error: Into<Error>,
{
    type Item = S::Response;
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        loop {
            *self = match self {
                ResponseFuture::Inner(ref mut f) => return f.poll().map_err(Into::into),
                ResponseFuture::Dispatch(ref mut f) => return f.poll().map_err(Into::into),
                ResponseFuture::Canonicalize {
                    ref mut rx,
                    ref mut inner,
                } => {
                    match rx.poll_ref().expect("watch cannot fail") {
                        Async::NotReady => return Ok(Async::NotReady),
                        Async::Ready(Some(update)) => match *update {
                            None => continue, // not set yet; loop to poll again
                            Some(ref na) => {
                                let (mut req, svc) = inner.take().expect("illegal state");
                                req.extensions_mut().insert(Addr::from(na.clone()));
                                ResponseFuture::Dispatch(svc.oneshot(req))
                            }
                        },
                        Async::Ready(None) => {
                            let (req, svc) = inner.take().expect("illegal state");
                            ResponseFuture::Dispatch(svc.oneshot(req))
                        }
                    }
                }
            }
        }
    }
}
