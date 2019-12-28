#![deny(warnings, rust_2018_idioms)]

use futures::{future, Async, Future, Poll};
use linkerd2_error::Error;
use tokio::sync::lock;
use tracing::trace;

pub trait CloneError {
    type Error: Into<Error> + Clone;

    fn clone_err(&self, err: Error) -> Self::Error;
}

#[derive(Clone, Debug)]
pub struct Layer<E = Poisoned> {
    _marker: std::marker::PhantomData<E>,
}

pub struct Lock<S, E = Poisoned> {
    lock: lock::Lock<State<S, E>>,
    locked: Option<lock::LockGuard<State<S, E>>>,
}

enum State<S, E> {
    Service(S),
    Error(E),
}

#[derive(Clone, Debug)]
pub struct Poisoned(String);

impl Layer {
    pub fn new() -> Self {
        Layer {
            _marker: std::marker::PhantomData,
        }
    }

    pub fn with_errors<E: Clone + From<Error> + Into<Error>>(self) -> Layer<E> {
        Layer {
            _marker: std::marker::PhantomData,
        }
    }
}

impl<S, E: From<Error> + Clone> tower::layer::Layer<S> for Layer<E> {
    type Service = Lock<S, E>;

    fn layer(&self, service: S) -> Self::Service {
        Self::Service {
            locked: None,
            lock: lock::Lock::new(State::Service(service)),
        }
    }
}

impl<S, E> Clone for Lock<S, E> {
    fn clone(&self) -> Self {
        Self {
            locked: None,
            lock: self.lock.clone(),
        }
    }
}

impl<T, S, E> tower::Service<T> for Lock<S, E>
where
    S: tower::Service<T>,
    S::Error: Into<Error>,
    E: Clone + From<Error> + Into<Error>,
{
    type Response = S::Response;
    type Error = Error;
    type Future = future::MapErr<S::Future, fn(S::Error) -> Error>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        loop {
            if let Some(state) = self.locked.as_mut() {
                if let State::Service(ref mut service) = **state {
                    return match service.poll_ready() {
                        Ok(ok) => Ok(ok),
                        Err(err) => {
                            let err = E::from(err.into());
                            **state = State::Error(err.clone());
                            self.locked = None;
                            Err(err.into())
                        }
                    };
                }

                unreachable!("must not lock on error");
            }

            let locked = match self.lock.poll_lock() {
                Async::Ready(locked) => locked,
                Async::NotReady => {
                    trace!("awaiting lock");
                    return Ok(Async::NotReady);
                }
            };
            trace!("locked; awaiting readiness");
            if let State::Error(ref e) = *locked {
                return Err(e.clone().into());
            }

            self.locked = Some(locked);
        }
    }

    fn call(&mut self, t: T) -> Self::Future {
        if let Some(mut state) = self.locked.take() {
            if let State::Service(ref mut service) = *state {
                return service.call(t).map_err(Into::into);
            }
        }

        unreachable!("called before ready");
    }
}

impl std::fmt::Display for Poisoned {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl std::error::Error for Poisoned {}

impl From<Error> for Poisoned {
    fn from(e: Error) -> Self {
        Poisoned(e.to_string())
    }
}
