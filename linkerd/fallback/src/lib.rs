#![deny(warnings, rust_2018_idioms)]

use futures::{try_ready, Future, Poll};
use linkerd2_error::Error;
use tower::util::Either;
use tracing::trace;

/// A fallback layer composing two service builders.
///
/// If the future returned by the primary builder's `MakeService` fails with
/// an error matching a given predicate, the fallback future will attempt
/// to call the secondary `MakeService`.
#[derive(Clone, Debug)]
pub struct Layer<A, B, R, P = fn(&Error) -> bool> {
    primary: A,
    fallback: B,
    predicate: P,
    _marker: std::marker::PhantomData<fn(R)>,
}

pub struct MakeSvc<A, B, P, R> {
    primary: A,
    fallback: B,
    predicate: P,
    _marker: std::marker::PhantomData<fn(R)>,
}

pub struct MakeFuture<A, B, P, T, R>
where
    A: Future,
    A::Item: tower::Service<R>,
    A::Error: Into<Error>,
    B: tower::MakeService<T, R, Response = <A::Item as tower::Service<R>>::Response>,
{
    fallback: B,
    target: Option<T>,
    predicate: P,
    state: FallbackState<A, B::Future, T>,
}

enum FallbackState<A, B, T> {
    /// Waiting for the primary service's future to complete.
    Primary(A),
    ///W aiting for the fallback service to become ready.
    Waiting(Option<T>),
    /// Waiting for the fallback service's future to complete.
    Fallback(B),
}

pub fn layer<A, B, R>(primary: A, fallback: B) -> Layer<A, B, R> {
    let predicate: fn(&Error) -> bool = |_| true;
    Layer {
        primary,
        fallback,
        predicate,
        _marker: std::marker::PhantomData,
    }
}

// === impl Layer ===

impl<A, B, R> Layer<A, B, R> {
    /// Returns a `Layer` that uses the given `predicate` to determine whether
    /// to fall back.
    pub fn with_predicate<P>(self, predicate: P) -> Layer<A, B, R, P>
    where
        P: Fn(&Error) -> bool + Clone,
    {
        Layer {
            predicate,
            primary: self.primary,
            fallback: self.fallback,
            _marker: self._marker,
        }
    }

    /// Returns a `Layer` that falls back if the error or its source is of
    /// type `E`.
    pub fn on_error<E>(self) -> Layer<A, B, R>
    where
        E: std::error::Error + 'static,
    {
        self.with_predicate(|e| e.is::<E>() || e.source().map(|s| s.is::<E>()).unwrap_or(false))
    }
}

impl<A, B, P, M, R> tower::layer::Layer<M> for Layer<A, B, R, P>
where
    A: tower::layer::Layer<M>,
    B: tower::layer::Layer<M>,
    M: Clone,
    P: Fn(&Error) -> bool + Clone,
{
    type Service = MakeSvc<A::Service, B::Service, P, R>;

    fn layer(&self, inner: M) -> Self::Service {
        MakeSvc {
            primary: self.primary.layer(inner.clone()),
            fallback: self.fallback.layer(inner),
            predicate: self.predicate.clone(),
            _marker: self._marker,
        }
    }
}

// === impl MakeSvc ===

impl<A, B, P, T, R> tower::Service<T> for MakeSvc<A, B, P, R>
where
    A: tower::MakeService<T, R>,
    A::MakeError: Into<Error>,
    A::Error: Into<Error>,
    B: tower::MakeService<T, R, Response = A::Response> + Clone,
    B::MakeError: Into<Error>,
    B::Error: Into<Error>,
    P: Fn(&Error) -> bool + Clone,
    T: Clone,
{
    type Response = Either<A::Service, B::Service>;
    type Error = Error;
    type Future = MakeFuture<A::Future, B, P, T, R>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.primary.poll_ready().map_err(Into::into)
    }

    fn call(&mut self, target: T) -> Self::Future {
        MakeFuture {
            fallback: self.fallback.clone(),
            predicate: self.predicate.clone(),
            target: Some(target.clone()),
            state: FallbackState::Primary(self.primary.make_service(target)),
        }
    }
}

impl<A, B, P, R> Clone for MakeSvc<A, B, P, R>
where
    A: Clone,
    B: Clone,
    P: Clone,
{
    fn clone(&self) -> Self {
        Self {
            primary: self.primary.clone(),
            fallback: self.fallback.clone(),
            predicate: self.predicate.clone(),
            _marker: self._marker,
        }
    }
}

impl<A, B, P, T, R> Future for MakeFuture<A, B, P, T, R>
where
    A: Future,
    A::Item: tower::Service<R>,
    A::Error: Into<Error>,
    B: tower::MakeService<T, R, Response = <A::Item as tower::Service<R>>::Response>,
    B::MakeError: Into<Error>,
    B::Error: Into<Error>,
    P: Fn(&Error) -> bool,
{
    type Item = Either<A::Item, B::Service>;
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        loop {
            self.state = match self.state {
                // We've called the primary service and are waiting for its
                // future to complete.
                FallbackState::Primary(ref mut f) => match f.poll() {
                    Ok(r) => return Ok(r.map(Either::A)),
                    Err(error) => {
                        let error = error.into();
                        if (self.predicate)(&error) {
                            trace!("{} matches; trying to fall back", error);
                            FallbackState::Waiting(self.target.take())
                        } else {
                            trace!("{} does not match; not falling back", error);
                            return Err(error);
                        }
                    }
                },
                // The primary service has returned an error matching the
                // predicate, and we are waiting for the fallback service to be ready.
                FallbackState::Waiting(ref mut target) => {
                    try_ready!(self.fallback.poll_ready().map_err(Into::into));
                    let target = target.take().expect("target should only be taken once");
                    FallbackState::Fallback(self.fallback.make_service(target))
                }
                // We've called the fallback service and are waiting for its
                // future to complete.
                FallbackState::Fallback(ref mut f) => {
                    return f.poll().map(|a| a.map(Either::B)).map_err(Into::into)
                }
            }
        }
    }
}
