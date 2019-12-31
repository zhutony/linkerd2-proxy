#![deny(warnings, rust_2018_idioms)]

linkerd2_cache::Service as Cache;

pub struct ResultCache<T, M> {
    cache: Cache<T, M>,
}
