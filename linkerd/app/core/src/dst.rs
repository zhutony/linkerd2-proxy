use super::classify;
use crate::profiles;
use linkerd2_addr::Addr;
use linkerd2_http_classify::CanClassify;
use linkerd2_proxy_http::timeout;
use std::fmt;
use std::time::Duration;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Route {
    pub target: Addr,
    pub route: profiles::Route,
    pub direction: super::metric_labels::Direction,
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
