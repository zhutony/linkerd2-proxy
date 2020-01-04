#![deny(warnings, rust_2018_idioms)]

pub mod layer;
pub mod make;
pub mod map_response;
pub mod map_target;
pub mod oneshot;
pub mod pending;
pub mod per_make;
pub mod proxy;
mod shared;

pub use self::{
    layer::{Layer, LayerExt},
    make::Make,
    proxy::Proxy,
    shared::Shared,
};
