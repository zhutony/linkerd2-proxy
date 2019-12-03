use futures::{future, try_ready, Future, Poll};
use linkerd2_duplex::Duplex;
use linkerd2_error::Error;
use tokio::io::{AsyncRead, AsyncWrite};
use tower::Service;

#[derive(Clone, Debug)]
pub struct Layer(());

#[derive(Clone, Debug)]
pub struct MakeForward<C> {
    connect: C,
}

#[derive(Clone, Debug)]
pub struct Forward<C, T> {
    connect: C,
    target: T,
}

pub enum ForwardFuture<I, F: Future> {
    Connect { connect: F, io: Option<I> },
    Duplex(Duplex<I, F::Item>),
}

impl Layer {
    pub fn new() -> Self {
        Layer(())
    }
}

impl<C> tower::layer::Layer<C> for Layer {
    type Service = MakeForward<C>;

    fn layer(&self, connect: C) -> Self::Service {
        Self::Service { connect }
    }
}

impl<C, T> Service<T> for MakeForward<C>
where
    C: tower::Service<T> + Clone,
    C::Error: Into<Error>,
    C::Response: AsyncRead + AsyncWrite,
{
    type Response = Forward<C, T>;
    type Error = Error;
    type Future = future::FutureResult<Self::Response, Self::Error>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        Ok(().into())
    }

    fn call(&mut self, target: T) -> Self::Future {
        future::ok(Forward {
            connect: self.connect.clone(),
            target,
        })
    }
}

impl<C, T, I> Service<I> for Forward<C, T>
where
    T: Clone,
    C: Service<T>,
    C::Error: Into<Error>,
    C::Response: AsyncRead + AsyncWrite,
    I: AsyncRead + AsyncWrite,
{
    type Response = ();
    type Error = Error;
    type Future = ForwardFuture<I, C::Future>;

    fn poll_ready(&mut self) -> Poll<(), self::Error> {
        self.connect.poll_ready().map_err(Into::into)
    }

    fn call(&mut self, io: I) -> Self::Future {
        ForwardFuture::Connect {
            io: Some(io),
            connect: self.connect.call(self.target.clone()),
        }
    }
}

impl<I, F> Future for ForwardFuture<I, F>
where
    I: AsyncRead + AsyncWrite,
    F: Future,
    F::Item: AsyncRead + AsyncWrite,
    F::Error: Into<Error>,
{
    type Item = ();
    type Error = Error;

    fn poll(&mut self) -> Poll<(), Self::Error> {
        loop {
            *self = match self {
                ForwardFuture::Connect {
                    ref mut connect,
                    ref mut io,
                } => {
                    let client_io = try_ready!(connect.poll().map_err(Into::into));
                    let server_io = io.take().expect("illegal state");
                    ForwardFuture::Duplex(Duplex::new(server_io, client_io))
                }
                ForwardFuture::Duplex(ref mut fut) => {
                    return fut.poll().map_err(Into::into);
                }
            }
        }
    }
}
