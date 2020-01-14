use indexmap::IndexMap;
use linkerd2_app_core::{
    dst, metric_labels,
    metric_labels::{prefix_labels, EndpointLabels},
    proxy::{
        api_resolve::{Metadata, ProtocolHint},
        http::{self, identity_from_header, profiles},
        identity,
        resolve::map_endpoint::MapEndpoint,
        tap,
    },
    router,
    transport::{connect, tls},
    Addr, Conditional, L5D_REQUIRE_ID,
};
use std::net::SocketAddr;
use std::sync::Arc;

#[derive(Copy, Clone, Debug)]
pub struct FromMetadata;

pub type Logical = Target<HttpEndpoint>;

pub type Concrete = Target<Logical>;

#[derive(Clone, Debug)]
pub struct RequestTarget(tls::accept::Meta);

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Profile(Addr);

#[derive(Clone, Debug)]
pub struct TargetProfile;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Target<T> {
    pub addr: Addr,
    pub inner: T,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HttpEndpoint {
    pub addr: SocketAddr,
    pub settings: http::Settings,
    pub identity: tls::PeerIdentity,
    pub metadata: Metadata,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TcpEndpoint {
    pub addr: SocketAddr,
    pub identity: tls::PeerIdentity,
}

impl<T> Target<T> {
    pub fn map<U, F: Fn(T) -> U>(self, map: F) -> Target<U> {
        Target {
            addr: self.addr,
            inner: (map)(self.inner),
        }
    }
}

// === impl HttpEndpoint ===

impl HttpEndpoint {
    pub fn can_use_orig_proto(&self) -> bool {
        if let ProtocolHint::Unknown = self.metadata.protocol_hint() {
            return false;
        }

        match self.settings {
            http::Settings::Http2 => false,
            http::Settings::Http1 {
                keep_alive: _,
                wants_h1_upgrade,
                was_absolute_form: _,
            } => !wants_h1_upgrade,
        }
    }
}

impl std::fmt::Display for HttpEndpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.addr.fmt(f)
    }
}

impl std::hash::Hash for HttpEndpoint {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.addr.hash(state);
        self.identity.hash(state);
        self.settings.hash(state);
        // Ignore metadata.
    }
}

impl tls::HasPeerIdentity for HttpEndpoint {
    fn peer_identity(&self) -> tls::PeerIdentity {
        self.identity.clone()
    }
}

impl connect::ConnectAddr for HttpEndpoint {
    fn connect_addr(&self) -> SocketAddr {
        self.addr
    }
}

impl http::settings::HasSettings for HttpEndpoint {
    fn http_settings(&self) -> &http::Settings {
        &self.settings
    }
}

impl<T:  http::settings::HasSettings> http::settings::HasSettings for Target<T> {
    fn http_settings(&self) -> &http::Settings {
        self.inner.http_settings()
    }
}


impl<T: http::settings::HasSettings> http::normalize_uri::ShouldNormalizeUri for Target<T> {
    fn should_normalize_uri(&self) -> Option<http::uri::Authority> {
        if let http::Settings::Http1 {
            was_absolute_form: false,
            ..
        } = *self.inner.http_settings()
        {
            return Some(self.addr.to_http_authority());
        }
        None
    }
}

impl tap::Inspect for HttpEndpoint {
    fn src_addr<B>(&self, req: &http::Request<B>) -> Option<SocketAddr> {
        req.extensions()
            .get::<tls::accept::Meta>()
            .map(|s| s.addrs.peer())
    }

    fn src_tls<'a, B>(
        &self,
        _: &'a http::Request<B>,
    ) -> Conditional<&'a identity::Name, tls::ReasonForNoIdentity> {
        Conditional::None(tls::ReasonForNoPeerName::Loopback.into())
    }

    fn dst_addr<B>(&self, _: &http::Request<B>) -> Option<SocketAddr> {
        Some(self.addr)
    }

    fn dst_labels<B>(&self, _: &http::Request<B>) -> Option<&IndexMap<String, String>> {
        Some(self.metadata.labels())
    }

    fn dst_tls<B>(
        &self,
        _: &http::Request<B>,
    ) -> Conditional<&identity::Name, tls::ReasonForNoIdentity> {
        self.identity.as_ref()
    }

    fn route_labels<B>(&self, req: &http::Request<B>) -> Option<Arc<IndexMap<String, String>>> {
        req.extensions()
            .get::<dst::Route>()
            .map(|r| r.route.labels().clone())
    }

    fn is_outbound<B>(&self, _: &http::Request<B>) -> bool {
        true
    }
}

impl MapEndpoint<Target<http::Settings>, Metadata> for FromMetadata {
    type Out = Target<HttpEndpoint>;

    fn map_endpoint(
        &self,
        target: &Target<http::Settings>,
        addr: SocketAddr,
        metadata: Metadata,
    ) -> HttpEndpoint {
        let identity = metadata
            .identity()
            .cloned()
            .map(Conditional::Some)
            .unwrap_or_else(|| {
                Conditional::None(tls::ReasonForNoPeerName::NotProvidedByServiceDiscovery.into())
            });

        target.clone().map(|settings| HttpEndpoint {
            addr,
            identity,
            metadata,
            settings,
        })
    }
}

impl Into<EndpointLabels> for Target<HttpEndpoint> {
    fn into(self) -> EndpointLabels {
        use linkerd2_app_core::metric_labels::{Direction, TlsId};
        EndpointLabels {
            authority: Some(self.addr.to_http_authority()),
            direction: Direction::Out,
            tls_id: self.identity.as_ref().map(|id| TlsId::ServerId(id.clone())),
            labels: prefix_labels("dst", self.metadata.labels().into_iter()),
        }
    }
}

// === impl TcpEndpoint ===

impl From<SocketAddr> for TcpEndpoint {
    fn from(addr: SocketAddr) -> Self {
        Self {
            addr,
            identity: Conditional::None(tls::ReasonForNoPeerName::NotHttp.into()),
        }
    }
}

impl connect::ConnectAddr for TcpEndpoint {
    fn connect_addr(&self) -> SocketAddr {
        self.addr
    }
}

impl tls::HasPeerIdentity for TcpEndpoint {
    fn peer_identity(&self) -> tls::PeerIdentity {
        self.identity.clone()
    }
}

impl Into<EndpointLabels> for TcpEndpoint {
    fn into(self) -> EndpointLabels {
        use linkerd2_app_core::metric_labels::{Direction, TlsId};
        EndpointLabels {
            direction: Direction::Out,
            tls_id: self.identity.as_ref().map(|id| TlsId::ServerId(id.clone())),
            authority: None,
            labels: None,
        }
    }
}

// === impl Concrete ===

impl std::fmt::Display for Concrete {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.dst.fmt(f)
    }
}

// === impl LogicalOrFallbackTarget ===

impl From<tls::accept::Meta> for RequestTarget {
    fn from(accept: tls::accept::Meta) -> Self {
        RequestTarget(accept)
    }
}

impl<B> router::Key<http::Request<B>> for RequestTarget {
    type Key = Target<HttpEndpoint>;

    fn key(&self, req: &http::Request<B>) -> Self::Key {
        use linkerd2_app_core::{
            http_request_authority_addr, http_request_host_addr, http_request_l5d_override_dst_addr,
        };

        let addr = http_request_l5d_override_dst_addr(req)
            .map(|addr| {
                tracing::debug!(%addr, "using dst-override");
                addr
            })
            .or_else(|_| {
                http_request_authority_addr(req).map(|addr| {
                    tracing::debug!(%addr, "using authority");
                    addr
                })
            })
            .or_else(|_| {
                http_request_host_addr(req).map(|addr| {
                    tracing::debug!(%addr, "using host");
                    addr
                })
            })
            .unwrap_or_else(|_| {
                let addr = self.0.addrs.target_addr();
                tracing::debug!(%addr, "using socket target");
                addr.into()
            });

        let settings = http::Settings::from_request(req);

        tracing::debug!(headers = ?req.headers(), uri = %req.uri(), target.addr = %addr, http.settings = ?settings, "Setting target for request");

        let inner = HttpEndpoint {
            settings,
            addr: self.0.addrs.target_addr(),
            metadata: Metadata::empty(),
            identity: identity_from_header(req, L5D_REQUIRE_ID)
                .map(Conditional::Some)
                .unwrap_or_else(|| {
                    Conditional::None(
                        tls::ReasonForNoPeerName::NotProvidedByServiceDiscovery.into(),
                    )
                }),
        };

        Target { addr, inner }
    }
}

impl<T> http::canonicalize::Target for Target<T> {
    fn addr(&self) -> &Addr {
        &self.addr
    }

    fn addr_mut(&mut self) -> &mut Addr {
        &mut self.addr
    }
}

impl<'t, T> From<&'t Target<T>> for ::http::header::HeaderValue {
    fn from(target: &'t Target<T>) -> Self {
        ::http::header::HeaderValue::from_str(&target.addr.to_string())
            .expect("addr must be a valid header")
    }
}

impl http::normalize_uri::ShouldNormalizeUri for Target<HttpEndpoint> {
    fn should_normalize_uri(&self) -> Option<http::uri::Authority> {
        if let http::Settings::Http1 {
            was_absolute_form: false,
            ..
        } = self.inner.settings
        {
            return Some(self.addr.to_http_authority());
        }
        None
    }
}

impl<T> profiles::OverrideDestination for Target<T> {
    fn dst_mut(&mut self) -> &mut Addr {
        &mut self.addr
    }
}

impl From<Logical> for Concrete {
    fn from(logical: Logical) -> Self {
        Concrete {
            dst: logical.dst.clone(),
            logical,
        }
    }
}

// === impl ProfileTarget ===

impl<T> router::Key<Target<T>> for TargetProfile {
    type Key = Profile;

    fn key(&self, t: &Target<T>) -> Self::Key {
        Profile(t.addr.clone())
    }
}

impl profiles::HasDestination for Profile {
    fn destination(&self) -> Addr {
        self.0.clone()
    }
}

impl profiles::WithRoute for Profile {
    type Route = dst::Route;

    fn with_route(self, route: profiles::Route) -> Self::Route {
        dst::Route {
            route,
            target: self.0.clone(),
            direction: metric_labels::Direction::Out,
        }
    }
}
