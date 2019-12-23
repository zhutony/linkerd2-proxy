#![deny(warnings, rust_2018_idioms)]

use futures::{future, try_ready, Future, Poll};
use linkerd2_error::Error;
use tokio::sync::lock;

pub trait CloneError {
    type Error: Into<Error> + Clone;

    fn clone_err(&self, err: &Error) -> Self::Error;
}

#[derive(Clone, Debug)]
pub struct Layer<C = ()> {
    clone_err: C,
}

pub struct Lock<S, C: CloneError = ()> {
    lock: lock::Lock<State<S, C>>,
    locked: Option<lock::LockGuard<State<S, C>>>,
}

enum State<S, C: CloneError> {
    Service { service: S, clone_err: C },
    Error(C::Error),
}

#[derive(Clone, Debug)]
pub struct Poisoned(String);

impl Layer {
    pub fn new() -> Self {
        Layer { clone_err: () }
    }
}

impl<S, C: Clone + CloneError> tower::layer::Layer<S> for Layer<C> {
    type Service = Lock<S, C>;

    fn layer(&self, service: S) -> Self::Service {
        let clone_err = self.clone_err.clone();
        Self::Service {
            locked: None,
            lock: lock::Lock::new(State::Service { service, clone_err }),
        }
    }
}

impl<S, C: CloneError> Clone for Lock<S, C> {
    fn clone(&self) -> Self {
        Self {
            locked: None,
            lock: self.lock.clone(),
        }
    }
}

impl<T, S, C> tower::Service<T> for Lock<S, C>
where
    S: tower::Service<T>,
    S::Error: Into<Error>,
    C: CloneError,
{
    type Response = S::Response;
    type Error = Error;
    type Future = future::MapErr<S::Future, fn(S::Error) -> Error>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        loop {
            if let Some(state) = self.locked.as_mut() {
                if let State::Service {
                    ref mut service,
                    ref clone_err,
                } = **state
                {
                    return match service.poll_ready() {
                        Ok(ok) => Ok(ok),
                        Err(err) => {
                            let err: Error = err.into();
                            **state = State::Error(clone_err.clone_err(&err));
                            self.locked = None;
                            return Err(err);
                        }
                    };
                }

                unreachable!("must not lock on error");
            }

            let locked = try_ready!(Ok::<_, Error>(self.lock.poll_lock()));
            if let State::Error(ref e) = *locked {
                return Err(e.clone().into());
            }

            self.locked = Some(locked);
        }
    }

    fn call(&mut self, t: T) -> Self::Future {
        if let Some(mut state) = self.locked.take() {
            if let State::Service {
                ref mut service, ..
            } = *state
            {
                return service.call(t).map_err(Into::into);
            }
        }

        unreachable!("called before ready")
    }
}

impl CloneError for () {
    type Error = Poisoned;

    fn clone_err(&self, e: &Error) -> Self::Error {
        Poisoned(e.to_string())
    }
}

impl std::fmt::Display for Poisoned {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl std::error::Error for Poisoned {}
