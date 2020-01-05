//! Configures and runs the outbound proxy.
//!
//! The outound proxy is responsible for routing traffic from the local
//! application to external network endpoints.

#![deny(warnings, rust_2018_idioms)]

pub use self::endpoint::{
    Concrete, HttpEndpoint, Logical, LogicalOrFallbackTarget, Profile, ProfileTarget, TcpEndpoint,
};
use futures::future;
use linkerd2_app_core::{
    classify,
    config::{ProxyConfig, ServerConfig},
    dns, drain, dst, errors,
    opencensus::proto::trace::v1 as oc,
    proxy::{
        self,
        core::resolve::Resolve,
        discover, fallback,
        http::{self, profiles},
        identity,
        resolve::map_endpoint,
        tap, tcp, Server,
    },
    reconnect, router, serve,
    spans::SpanConverter,
    svc::{self, Make},
    trace, trace_context,
    transport::{self, connect, tls, OrigDstAddr, SysOrigDstAddr},
    Conditional, Error, ProxyMetrics, CANONICAL_DST_HEADER, DST_OVERRIDE_HEADER, L5D_CLIENT_ID,
    L5D_REMOTE_IP, L5D_REQUIRE_ID, L5D_SERVER_ID,
};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::info_span;

#[allow(dead_code)] // TODO #2597
mod add_remote_ip_on_rsp;
#[allow(dead_code)] // TODO #2597
mod add_server_id_on_rsp;
mod endpoint;
mod orig_proto_upgrade;
mod require_identity_on_endpoint;

const EWMA_DEFAULT_RTT: Duration = Duration::from_millis(30);
const EWMA_DECAY: Duration = Duration::from_secs(10);

#[derive(Clone, Debug)]
pub struct Config<A: OrigDstAddr = SysOrigDstAddr> {
    pub proxy: ProxyConfig<A>,
    pub canonicalize_timeout: Duration,
}

pub struct Outbound {
    pub listen_addr: SocketAddr,
    pub serve: serve::Task,
}

impl<A: OrigDstAddr> Config<A> {
    pub fn with_orig_dst_addr<B: OrigDstAddr>(self, orig_dst_addr: B) -> Config<B> {
        Config {
            proxy: self.proxy.with_orig_dst_addr(orig_dst_addr),
            canonicalize_timeout: self.canonicalize_timeout,
        }
    }

    pub fn build<R, P>(
        self,
        local_identity: tls::Conditional<identity::Local>,
        resolve: R,
        dns_resolver: dns::Resolver,
        profiles_client: P,
        tap_layer: tap::Layer,
        metrics: ProxyMetrics,
        span_sink: Option<mpsc::Sender<oc::Span>>,
        drain: drain::Watch,
    ) -> Result<Outbound, Error>
    where
        A: Send + 'static,
        R: Resolve<Concrete, Endpoint = proxy::api_resolve::Metadata>
            + Clone
            + Send
            + Sync
            + 'static,
        R::Future: Send,
        R::Resolution: Send,
        P: profiles::GetRoutes<Profile> + Clone + Send + 'static,
        P::Future: Send,
    {
        use proxy::core::listen::{Bind, Listen};
        let Config {
            canonicalize_timeout,
            proxy:
                ProxyConfig {
                    server: ServerConfig { bind, h2_settings },
                    connect,
                    cache_capacity,
                    cache_max_idle_age,
                    disable_protocol_detection_for_ports,
                    service_acquisition_timeout,
                    max_in_flight_requests,
                },
        } = self;

        let listen = bind.bind().map_err(Error::from)?;
        let listen_addr = listen.listen_addr();

        // The stack is served lazily since caching layers spawn tasks from
        // their constructor. This helps to ensure that tasks are spawned on the
        // same runtime as the proxy.
        let serve = Box::new(future::lazy(move || {
            // Establishes connections to remote peers (for both TCP
            // forwarding and HTTP proxying).
            let tcp_connect = svc::stack(connect::Connect::new(connect.keepalive))
                // Initiates mTLS if the target is configured with identity.
                .push(tls::client::layer(local_identity))
                // Limits the time we wait for a connection to be established.
                .push_timeout(connect.timeout);

            // Forwards TCP streams that cannot be decoded as HTTP.
            let tcp_forward = tcp_connect
                .clone()
                .push(metrics.transport.layer_connect(TransportLabels))
                .push(svc::map_target::layer(|meta: tls::accept::Meta| {
                    TcpEndpoint::from(meta.addrs.target_addr())
                }))
                .push(svc::layer::mk(tcp::Forward::new));

            // Registers the stack with Tap, Metrics, and OpenCensus tracing export.
            let http_endpoint_observability = svc::layers()
                .push(tap_layer.clone())
                .push(http::metrics::layer::<_, classify::Response>(
                    metrics.http_endpoint.clone(),
                ))
                .push_per_make(trace_context::layer(
                    span_sink
                        .clone()
                        .map(|sink| SpanConverter::client(sink, trace_labels())),
                ));

            // Checks the headers to validate that a client-specified required
            // identity matches the configured identity.
            let http_endpoint_identity_headers = svc::layers()
                .push_per_make(
                    svc::layers()
                        .push(http::strip_header::response::layer(L5D_REMOTE_IP))
                        .push(http::strip_header::response::layer(L5D_SERVER_ID))
                        .push(http::strip_header::request::layer(L5D_REQUIRE_ID)),
                )
                .push(require_identity_on_endpoint::layer());

            let http_balancer_endpoint = tcp_connect
                .clone()
                .push(metrics.transport.layer_connect(TransportLabels))
                // Initiates an HTTP client on the underlying transport.
                // Prior-knowledge HTTP/2 is typically used (i.e. when
                // communicating with other proxies); though HTTP/1.x fallback
                // is supported as needed.
                .push(http::client::layer(connect.h2_settings))
                // Reestablishes a connection when the client fails.
                .push(reconnect::layer({
                    let backoff = connect.backoff.clone();
                    move |_| Ok(backoff.stream())
                }))
                .push(http_endpoint_observability.clone())
                .push(http_endpoint_identity_headers.clone())
                // Upgrades HTTP/1 requests to be transported over HTTP/2
                // connections.
                //
                // This sets headers so that the inbound proxy can downgrade the
                // request properly.
                .push(orig_proto_upgrade::layer())
                .serves::<HttpEndpoint>()
                .push(trace::layer(|endpoint: &HttpEndpoint| {
                    info_span!("endpoint", peer.addr = %endpoint.addr, peer.id = ?endpoint.identity)
                }))
                .serves::<HttpEndpoint>();

            // Resolves each target via the control plane, producing a over all
            // endpoints returned from the destination service.
            //
            // This
            let discover = {
                const BUFFER_CAPACITY: usize = 10;
                let resolve = map_endpoint::Resolve::new(endpoint::FromMetadata, resolve.clone());
                discover::Layer::new(BUFFER_CAPACITY, cache_max_idle_age, resolve)
            };

            // If the balancer fails to be created, i.e., because it is unresolvable,
            // fall back to using a router that dispatches request to the
            // application-selected original destination.
            let http_balancer_cache = http_balancer_endpoint
                .push_spawn_ready()
                .push(discover)
                .push_per_make(http::balance::layer(EWMA_DEFAULT_RTT, EWMA_DECAY))
                .serves::<Concrete>()
                .push_pending()
                .push_per_make(svc::lock::Layer::new().with_errors::<LogicalError>())
                .spawn_cache(cache_capacity, cache_max_idle_age)
                .push(trace::layer(
                    |c: &Concrete| info_span!("balance", addr = %c.dst),
                ))
                .serves::<Concrete>();

            let http_profile_route_proxy = svc::stack(())
                .push(http::metrics::layer::<_, classify::Response>(
                    metrics.http_route_retry.clone(),
                ))
                .makes_clone::<dst::Route>()
                .push(http::retry::layer(metrics.http_route_retry))
                .makes::<dst::Route>()
                .push(http::timeout::layer())
                .push(http::metrics::layer::<_, classify::Response>(
                    metrics.http_route,
                ))
                .push(classify::layer());

            let http_profile_cache = http_balancer_cache
                .serves::<Concrete>()
                .push(http::profiles::Layer::with_overrides(
                    profiles_client,
                    http_profile_route_proxy.into_inner(),
                ))
                .push_pending()
                .push_per_make(svc::map_target::layer(Concrete::from))
                .push_per_make(svc::lock::Layer::new().with_errors::<LogicalError>())
                .spawn_cache(cache_capacity, cache_max_idle_age)
                .push(trace::layer(|_: &Profile| info_span!("profile")))
                .serves::<Profile>();

            // Caches DNS refinements from relative names to canonical names.
            //
            // For example, a client may send requests to `foo` or `foo.ns`; and
            // the canonical form of these names is `foo.ns.svc.cluster.local
            let dns_refine_cache = svc::stack(dns_resolver.into_make_refine())
                .push_per_make(svc::lock::Layer::new())
                .spawn_cache(cache_capacity, cache_max_idle_age)
                .push(trace::layer(
                    |name: &dns::Name| info_span!("canonicalize", %name),
                ))
                // Obtains the lock, advances the state of the resolution
                .push(svc::make_response::Layer)
                // Ensures that the cache isn't locked when polling readiness.
                .push_oneshot()
                .serves_rsp::<dns::Name, dns::Name>()
                .into_inner();

            let http_logical_router = {
                let profile_router = http_profile_cache
                    .push(router::Layer::new(|()| ProfileTarget))
                    .routes::<(), Logical>()
                    .make(());

                svc::stack(profile_router)
                    .serves::<Logical>()
                    .push(http::normalize_uri::layer())
                    .push(http::header_from_target::layer(CANONICAL_DST_HEADER))
                    .push(http::canonicalize::Layer::new(
                        dns_refine_cache,
                        canonicalize_timeout,
                    ))
                    .push_per_make(
                        svc::layers()
                            .push(http::strip_header::request::layer(L5D_CLIENT_ID))
                            .push(http::strip_header::request::layer(DST_OVERRIDE_HEADER)),
                    )
                    .push(trace::layer(
                        |logical: &Logical| info_span!("logical", addr = %logical.dst),
                    ))
                    .serves::<Logical>()
            };

            // Caches clients that bypass discovery/balancing.
            //
            // This is effectively the same as the endpoint stack; but the
            // client layer captures the requst body type (via PhantomData), so
            // the stack cannot be shared directly.
            let http_forward_cache = tcp_connect
                .push(metrics.transport.layer_connect(TransportLabels))
                .push(http::client::layer(connect.h2_settings))
                .push(reconnect::layer({
                    let backoff = connect.backoff.clone();
                    move |_| Ok(backoff.stream())
                }))
                .push(http::normalize_uri::layer())
                .push(http_endpoint_observability.clone())
                .push(http_endpoint_identity_headers.clone())
                .push_pending()
                .push_per_make(svc::lock::Layer::new())
                .spawn_cache(cache_capacity, cache_max_idle_age)
                .push(trace::layer(|endpoint: &HttpEndpoint| {
                    info_span!("bypass", peer.addr = %endpoint.addr, peer.id = ?endpoint.identity)
                }))
                .serves::<HttpEndpoint>();

            let http_admit_request = svc::layers()
                .push_oneshot()
                .push_concurrency_limit(max_in_flight_requests)
                .push_load_shed()
                .push(errors::layer())
                .push(trace_context::layer(span_sink.map(|span_sink| {
                    SpanConverter::server(span_sink, trace_labels())
                })))
                .push(metrics.http_handle_time.layer());

            let http_server = http_logical_router
                .push_make_ready()
                .push(svc::map_target::layer(|(l, _): (Logical, HttpEndpoint)| l))
                .push_per_make(svc::layers().boxed())
                .push(fallback::layer(
                    http_forward_cache
                        .push_make_ready()
                        .push(svc::map_target::layer(|(_, e): (Logical, HttpEndpoint)| e))
                        .push_per_make(svc::layers().boxed())
                        .into_inner()
                ).with_predicate(LogicalError::is_discovery_rejected))
                .serves::<(Logical, HttpEndpoint)>()
                .push_make_ready()
                .push_timeout(service_acquisition_timeout)
                .push(router::Layer::new(LogicalOrFallbackTarget::from))
                .push_per_make(http_admit_request)
                .push(trace::layer(
                    |src: &tls::accept::Meta| {
                        info_span!("source", target.addr = %src.addrs.target_addr())
                    },
                ))
                .makes::<tls::accept::Meta>();

            let tcp_server = Server::new(
                TransportLabels,
                metrics.transport,
                tcp_forward.into_inner(),
                http_server.into_inner(),
                h2_settings,
                drain.clone(),
                disable_protocol_detection_for_ports.clone(),
            );

            // The local application does not establish mTLS with the proxy.
            let no_tls: tls::Conditional<identity::Local> =
                Conditional::None(tls::ReasonForNoPeerName::Loopback.into());
            let accept = tls::AcceptTls::new(no_tls, tcp_server)
                .with_skip_ports(disable_protocol_detection_for_ports);

            serve::serve(listen, accept, drain)
        }));

        Ok(Outbound { listen_addr, serve })
    }
}

#[derive(Copy, Clone, Debug)]
struct TransportLabels;

impl transport::metrics::TransportLabels<HttpEndpoint> for TransportLabels {
    type Labels = transport::labels::Key;

    fn transport_labels(&self, endpoint: &HttpEndpoint) -> Self::Labels {
        transport::labels::Key::connect("outbound", endpoint.identity.as_ref())
    }
}

impl transport::metrics::TransportLabels<TcpEndpoint> for TransportLabels {
    type Labels = transport::labels::Key;

    fn transport_labels(&self, endpoint: &TcpEndpoint) -> Self::Labels {
        transport::labels::Key::connect("outbound", endpoint.identity.as_ref())
    }
}

impl transport::metrics::TransportLabels<proxy::server::Protocol> for TransportLabels {
    type Labels = transport::labels::Key;

    fn transport_labels(&self, proto: &proxy::server::Protocol) -> Self::Labels {
        transport::labels::Key::accept("outbound", proto.tls.peer_identity.as_ref())
    }
}

pub fn trace_labels() -> HashMap<String, String> {
    let mut l = HashMap::new();
    l.insert("direction".to_string(), "outbound".to_string());
    l
}

#[derive(Clone, Debug)]
enum LogicalError {
    DiscoveryRejected,
    Inner(String),
}

impl std::fmt::Display for LogicalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LogicalError::DiscoveryRejected => write!(f, "discovery rejected"),
            LogicalError::Inner(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for LogicalError {}

impl From<Error> for LogicalError {
    fn from(orig: Error) -> Self {
        if let Some(inner) = orig.downcast_ref::<LogicalError>() {
            return inner.clone();
        }

        if orig.is::<DiscoveryRejected>() {
            return LogicalError::DiscoveryRejected;
        }

        LogicalError::Inner(orig.to_string())
    }
}

impl LogicalError {
    fn is_discovery_rejected(err: &Error) -> bool {
        tracing::trace!(?err, "is_discovery_rejected");
        if let Some(LogicalError::DiscoveryRejected) = err.downcast_ref::<LogicalError>() {
            return true;
        }

        false
    }
}

// === impl DiscoveryRejected ===

#[derive(Clone, Debug)]
pub struct DiscoveryRejected(());

impl DiscoveryRejected {
    pub fn new() -> Self {
        DiscoveryRejected(())
    }
}

impl std::fmt::Display for DiscoveryRejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "discovery rejected")
    }
}

impl std::error::Error for DiscoveryRejected {}
