use futures::{Async, Future, Poll};
use linkerd2_error::Error;
use tokio::sync::lock::Lock;
use tokio::sync::oneshot;
//use tracing_futures::Instrument;
use std::sync::{Arc, Weak};

mod queue;

pub struct Buffer<Req, S: tower::Service<Req>> {
    queue: queue::Push<Req, S::Future>,
    capacity: usize,
    daemon_handle: Weak<()>,
    _handle: Arc<()>,
}

pub struct Daemon<Req, S: tower::Service<Req>> {
    service: S,
    buffer: Lock<InFlightList<Req, S::Future>>,
    buffer_handle: Weak<()>,
    _handle: Arc<()>,
}

#[derive(Debug)]
pub struct LostDaemon(());

pub enum BufferFuture<Req, F> {
    Push {
        req: Option<Req>,
        buf: Lock<InFlightList<Req, F>>,
    },
    InFlight(InFlightFuture<F>),
}

impl<Req, S> tower::Service<Req> for Buffer<Req, S>
where
    S: tower::Service<Req>,
{
    type Response = S::Future;
    type Error = Error;
    type Future = BufferFuture<Req, S::Future>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        if self.daemon_handle.upgrade().is_none() {
            return Err(LostDaemon(()).into());
        }

        if let Async::Ready(mut buffer) = self.buffer.poll_lock() {
            if buffer.size < self.capacity {
                return Ok(Async::Ready(()));
            }
        }

        Ok(Async::NotReady)
    }

    fn call(&mut self, req: Req) -> Self::Future {
        BufferFuture::Push {
            req: Some(req),
            buf: self.buffer.clone(),
        }
    }
}

impl<Req, F> Future for BufferFuture<Req, F> {
    type Item = F;
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        loop {
            *self = match self {
                BufferFuture::Push { mut req, mut buf } => match buf.poll_lock() {
                    Async::NotReady => return Ok(Async::NotReady),
                    Async::Ready(mut buf) => {
                        let req = req.take().expect("illegal state");
                        let rx = buf.push(req);
                        BufferFuture::InFlight(rx)
                    }
                },
                BufferFuture::InFlight(mut rx) => return rx.poll().map_err(Into::into),
            }
        }
    }
}

impl std::fmt::Display for LostDaemon {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "buffer daemon task was lost")
    }
}

impl std::error::Error for LostDaemon {}
