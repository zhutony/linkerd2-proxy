use indexmap::IndexMap;
use linkerd2_app_core::{
    dst::Route,
    metric_labels::{prefix_labels, EndpointLabels},
    proxy::{
        api_resolve::{Metadata, ProtocolHint},
        http::{self, identity_from_header},
        identity,
        resolve::map_endpoint::MapEndpoint,
        tap,
    },
    router,
    transport::{connect, tls},
    Addr, Conditional, NameAddr, L5D_REQUIRE_ID,
};
use std::net::SocketAddr;
use std::sync::Arc;

#[derive(Copy, Clone, Debug)]
pub struct FromMetadata;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Logical {
    pub dst: Addr,
    pub settings: http::Settings,
}

#[derive(Clone, Debug)]
pub struct LogicalTarget(tls::accept::Meta);

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Profile(Addr);

#[derive(Clone, Debug)]
pub struct ProfileTarget;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Concrete {
    pub dst: Addr,
    pub logical: Logical,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Endpoint {
    pub addr: SocketAddr,
    pub identity: tls::PeerIdentity,
    pub concrete: Concrete,
    pub metadata: Metadata,
}

impl Endpoint {
    pub fn can_use_orig_proto(&self) -> bool {
        if let ProtocolHint::Unknown = self.metadata.protocol_hint() {
            return false;
        }

        match self.concrete.logical.settings {
            http::Settings::Http2 => false,
            http::Settings::Http1 {
                keep_alive: _,
                wants_h1_upgrade,
                was_absolute_form: _,
            } => !wants_h1_upgrade,
            http::Settings::NotHttp => {
                unreachable!(
                    "Endpoint::can_use_orig_proto called when NotHttp: {:?}",
                    self,
                );
            }
        }
    }
}

impl From<SocketAddr> for Endpoint {
    fn from(addr: SocketAddr) -> Self {
        Self {
            addr,
            dst_logical: None,
            dst_concrete: None,
            identity: Conditional::None(tls::ReasonForNoPeerName::NotHttp.into()),
            metadata: Metadata::empty(),
            http_settings: http::Settings::NotHttp,
        }
    }
}

impl std::fmt::Display for Endpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.addr.fmt(f)
    }
}

impl std::hash::Hash for Endpoint {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.dst_logical.hash(state);
        self.dst_concrete.hash(state);
        self.addr.hash(state);
        self.identity.hash(state);
        self.http_settings.hash(state);
        // Ignore metadata.
    }
}

impl tls::HasPeerIdentity for Endpoint {
    fn peer_identity(&self) -> tls::PeerIdentity {
        self.identity.clone()
    }
}

impl connect::HasPeerAddr for Endpoint {
    fn peer_addr(&self) -> SocketAddr {
        self.addr
    }
}

impl http::settings::HasSettings for Endpoint {
    fn http_settings(&self) -> &http::Settings {
        &self.concrete.logical.settings
    }
}

impl tap::Inspect for Endpoint {
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
        req.extensions().get::<Route>().map(|r| r.labels().clone())
    }

    fn is_outbound<B>(&self, _: &http::Request<B>) -> bool {
        true
    }
}

impl MapEndpoint<Concrete, Metadata> for FromMetadata {
    type Out = Endpoint;

    fn map_endpoint(&self, concrete: &Concrete, addr: SocketAddr, metadata: Metadata) -> Endpoint {
        let identity = metadata
            .identity()
            .cloned()
            .map(Conditional::Some)
            .unwrap_or_else(|| {
                Conditional::None(tls::ReasonForNoPeerName::NotProvidedByServiceDiscovery.into())
            });
        Endpoint {
            addr,
            identity,
            metadata,
            concrete: concrete.clone(),
        }
    }
}

impl Into<EndpointLabels> for Endpoint {
    fn into(self) -> EndpointLabels {
        use linkerd2_app_core::metric_labels::{Direction, TlsId};
        EndpointLabels {
            dst_logical: self.dst_logical,
            dst_concrete: self.dst_concrete,
            direction: Direction::Out,
            tls_id: self.identity.as_ref().map(|id| TlsId::ServerId(id.clone())),
            labels: prefix_labels("dst", self.metadata.labels().into_iter()),
        }
    }
}

impl std::fmt::Display for Concrete {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.dst.fmt(f)
    }
}

// === impl LogicalTarget ===

impl From<tls::accept::Meta> for LogicalTarget {
    fn from(accept: tls::accept::Meta) -> Self {
        LogicalTarget(accept)
    }
}

impl<B> router::Target<http::Request<B>> for LogicalTarget {
    type Target = Logical;

    fn target(&self, req: &http::Request<B>) -> Self::Target {
        use linkerd2_app_core::{
            http_request_authority_addr, http_request_host_addr, http_request_l5d_override_dst_addr,
        };
        let dst = http_request_l5d_override_dst_addr(req)
            .map(|override_addr| {
                tracing::debug!("using dst-override");
                override_addr
            })
            .or_else(|_| http_request_authority_addr(req))
            .or_else(|_| http_request_host_addr(req))
            .unwrap_or_else(|_| self.0.addrs.target_addr().into());

        let settings = http::Settings::from_request(req);

        Logical { dst, settings }
    }
}

// === impl ProfileTarget ===

impl router::Target<Logical> for ProfileTarget {
    type Target = Profile;

    fn target(&self, t: &Logical) -> Self::Target {
        Profile(t.dst.clone())
    }
}
