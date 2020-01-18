use super::classify;
use crate::profiles;
use http;
use linkerd2_addr::Addr;
use linkerd2_proxy_http::{
    metrics::classify::{CanClassify, Classify, ClassifyEos, ClassifyResponse},
    timeout,
};
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Route {
    pub target: Addr,
    pub route: profiles::Route,
    pub direction: super::metric_labels::Direction,
}

#[derive(Clone, Debug)]
pub struct Retry {
    budget: Arc<tower::retry::budget::Budget>,
    response_classes: profiles::ResponseClasses,
}

#[derive(Copy, Clone, Debug)]
pub enum Retryability {
    Exhausted,
    NotRetryable,
    Retryable,
}

// === impl Route ===

impl CanClassify for Route {
    type Classify = classify::Request;

    fn classify(&self) -> classify::Request {
        self.route.response_classes().clone().into()
    }
}

impl timeout::HasTimeout for Route {
    fn timeout(&self) -> Option<Duration> {
        self.route.timeout()
    }
}

impl fmt::Display for Route {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.target.fmt(f)
    }
}

// === impl Retry ===

impl Retry {
    pub fn retryability<A, B, E>(
        &self,
        req: &http::Request<A>,
        result: Result<&http::Response<B>, &E>,
    ) -> Retryability {
        let retryable = match result {
            Err(_) => false,
            Ok(rsp) => classify::Request::from(self.response_classes.clone())
                .classify(req)
                .start(rsp)
                .eos(None)
                .is_failure(),
        };

        if !retryable {
            self.budget.deposit();
            return Retryability::NotRetryable;
        }

        if self.budget.withdraw().is_err() {
            return Retryability::Exhausted;
        }

        Retryability::Retryable
    }
}

// impl<B: TryClone> TryClone for Request<B> {
//     fn try_clone(&self) -> Option<Self> {
//         if let Some(body) = self.body().try_clone() {
//             let mut clone = Request::new(body);
//             *clone.method_mut() = self.method().clone();
//             *clone.uri_mut() = self.uri().clone();
//             *clone.headers_mut() = self.headers().clone();
//             *clone.version_mut() = self.version();

//             if let Some(ext) = self.extensions().get::<tls::accept::Meta>() {
//                 clone.extensions_mut().insert(ext.clone());
//             }

//             // Count retries toward the request's total handle time.
//             if let Some(ext) = self.extensions().get::<handle_time::Tracker>() {
//                 clone.extensions_mut().insert(ext.clone());
//             }

//             Some(clone)
//         } else {
//             None
//         }
//     }
// }
