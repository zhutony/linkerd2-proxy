//! Configures and runs the outbound proxy.
//!
//! The outound proxy is responsible for routing traffic from the local
//! application to external network endpoints.

#![allow(warnings, rust_2018_idioms)]

pub use self::endpoint::{
    Concrete, Endpoint, Logical, LogicalOrFallbackTarget, Profile, ProfileTarget,
};
use futures::future;
use linkerd2_app_core::{
    self as core, cache, classify,
    config::{ProxyConfig, ServerConfig},
    dns, drain, dst, errors,
    opencensus::proto::trace::v1 as oc,
    proxy::{
        self, core::resolve::Resolve, discover, fallback, http, identity, resolve::map_endpoint,
        tap, tcp, Server,
    },
    reconnect, router, serve,
    spans::SpanConverter,
    svc::{self, Make},
    trace, trace_context,
    transport::{self, connect, tls, OrigDstAddr, SysOrigDstAddr},
    Addr, Conditional, DispatchDeadline, Error, ProxyMetrics, CANONICAL_DST_HEADER,
    DST_OVERRIDE_HEADER, L5D_CLIENT_ID, L5D_REMOTE_IP, L5D_REQUIRE_ID, L5D_SERVER_ID,
};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::Duration;
use tokio::sync::mpsc;
use tower_grpc::{self as grpc, generic::client::GrpcService};
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
        profiles_client: core::profiles::Client<P>,
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
        P: GrpcService<grpc::BoxBody> + Clone + Send + Sync + 'static,
        P::ResponseBody: Send,
        <P::ResponseBody as grpc::Body>::Data: Send,
        P::Future: Send,
    {
        use proxy::core::listen::{Bind, Listen};
        let Config {
            canonicalize_timeout,
            proxy:
                ProxyConfig {
                    server:
                        ServerConfig {
                            bind,
                            buffer,
                            h2_settings,
                        },
                    connect,
                    router_capacity,
                    router_max_idle_age,
                    disable_protocol_detection_for_ports,
                },
        } = self;

        let listen = bind.bind().map_err(Error::from)?;
        let listen_addr = listen.listen_addr();

        // The stack is served lazily since some layers (notably buffer) spawn
        // tasks from their constructor. This helps to ensure that tasks are
        // spawned on the same runtime as the proxy.
        let serve = Box::new(future::lazy(move || {
            // Establishes connections to remote peers (for both TCP
            // forwarding and HTTP proxying).
            let connect_stack = svc::stack(connect::svc(connect.keepalive))
                .push(tls::client::layer(local_identity))
                .push_timeout(connect.timeout)
                .push(metrics.transport.layer_connect(TransportLabels));

            let endpoint_layers = svc::layers()
                .push(tap_layer.clone())
                .push(http::metrics::layer::<_, classify::Response>(
                    metrics.http_endpoint.clone(),
                ))
                .push(
                    svc::layers()
                        .push(trace_context::layer(span_sink.clone().map(|span_sink| {
                            SpanConverter::client(span_sink, trace_labels())
                        })))
                        .push(http::strip_header::response::layer(L5D_REMOTE_IP))
                        .push(http::strip_header::response::layer(L5D_SERVER_ID))
                        .push(http::strip_header::request::layer(L5D_REQUIRE_ID))
                        .per_make(),
                )
                .push(require_identity_on_endpoint::layer())
                .push(trace::layer(|endpoint: &Endpoint| {
                    info_span!("endpoint", peer.addr = %endpoint.addr, peer.id = ?endpoint.identity)
                }));

            let endpoint_stack = connect_stack
                .clone()
                .push(http::client::layer(connect.h2_settings))
                .push(reconnect::layer({
                    let backoff = connect.backoff.clone();
                    move |_| Ok(backoff.stream())
                }))
                .push(http::normalize_uri::layer())
                .push(orig_proto_upgrade::layer())
                .push(endpoint_layers.clone())
                .serves::<Endpoint>();

            // The client layer captures the requst body type into a
            // phantomdata, so the stack cannot be shared directly between the
            // balance and fallback stacks.
            let fallback_cache = connect_stack
                .clone()
                .push(http::client::layer(connect.h2_settings))
                .push(reconnect::layer({
                    let backoff = connect.backoff.clone();
                    move |_| Ok(backoff.stream())
                }))
                .push(http::normalize_uri::layer())
                .push(endpoint_layers)
                .push(trace::layer(|_: &Endpoint| info_span!("fallback")))
                .push_pending()
                .push_per_make(svc::lock::Layer::new())
                .spawn_cache(router_capacity, router_max_idle_age)
                .serves::<Endpoint>();

            // Resolves the target via the control plane and balances requests
            // over all endpoints returned from the destination service.
            const DISCOVER_UPDATE_BUFFER_CAPACITY: usize = 10;

            // If the balancer fails to be created, i.e., because it is unresolvable,
            // fall back to using a router that dispatches request to the
            // application-selected original destination.
            let balancer_cache = endpoint_stack
                .clone()
                .push_spawn_ready()
                .push(discover::Layer::new(
                    DISCOVER_UPDATE_BUFFER_CAPACITY,
                    router_max_idle_age,
                    map_endpoint::Resolve::new(endpoint::FromMetadata, resolve.clone()),
                ))
                .push_per_make(http::balance::layer(EWMA_DEFAULT_RTT, EWMA_DECAY))
                .push(trace::layer(
                    |c: &Concrete| info_span!("balance", addr = %c.dst),
                ))
                .serves::<Concrete>()
                .push_pending()
                .push_per_make(svc::lock::Layer::new().with_errors::<LogicalError>())
                .spawn_cache(router_capacity, router_max_idle_age)
                .serves::<Concrete>();

            let profile_stack = balancer_cache
                .serves::<Concrete>()
                .push(http::profiles::Layer::with_overrides(
                    profiles_client,
                    svc::stack(())
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
                        .push(classify::layer())
                        .into_inner(),
                ))
                .serves::<Profile>()
                .push_pending()
                .push_per_make(svc::map_target::layer(Concrete::from))
                .push(router::Layer::new(|()| ProfileTarget))
                .routes::<(), Logical>()
                .make(());

            let logical_cache = svc::stack(profile_stack)
                .serves::<Logical>()
                .push(http::header_from_target::layer(CANONICAL_DST_HEADER))
                .push(http::canonicalize::Layer::new(
                    dns_resolver,
                    canonicalize_timeout,
                ))
                .push_pending()
                .push_per_make(svc::lock::Layer::new().with_errors::<LogicalError>())
                .spawn_cache(router_capacity, router_max_idle_age)
                .push_per_make(
                    svc::layers()
                        .push(http::strip_header::request::layer(L5D_CLIENT_ID))
                        .push(http::strip_header::request::layer(DST_OVERRIDE_HEADER)),
                )
                .push(trace::layer(
                    |logical: &Logical| info_span!("logical", addr = %logical.dst),
                ))
                .serves::<Logical>();

            let server_stack = svc::stack(logical_cache)
                .serves::<Logical>()
                .push_ready()
                .push(svc::map_target::layer(|(l, _): (Logical, Endpoint)| l))
                .push_per_make(svc::layers().boxed())
                .push(fallback::layer(
                    fallback_cache
                        .push_ready()
                        .push(svc::map_target::layer(|(_, e): (Logical, Endpoint)| e))
                        .push_per_make(svc::layers().boxed())
                        .into_inner()
                ).with_predicate(LogicalError::is_discovery_rejected))
                .serves::<(Logical, Endpoint)>()
                .push_pending()
                .push_per_make(
                    svc::layers()
                        .push_ready_timeout(Duration::from_secs(10))
                )
                .makes::<(Logical, Endpoint)>()
                .push(router::Layer::new(LogicalOrFallbackTarget::from))
                .push_per_make(
                    svc::layers()
                        .push_concurrency_limit(buffer.max_in_flight)
                        .push_load_shed()
                        .push(errors::layer())
                        .push(trace_context::layer(span_sink.map(|span_sink| {
                            SpanConverter::server(span_sink, trace_labels())
                        })))
                        .push(metrics.http_handle_time.layer())
                )
                .push(trace::layer(
                    |src: &tls::accept::Meta| {
                        info_span!("source", target.addr = %src.addrs.target_addr())
                    },
                ))
                .makes::<tls::accept::Meta>();

            let forward_tcp = tcp::Forward::new(
                svc::stack(connect_stack)
                    .push(svc::map_target::layer(|meta: tls::accept::Meta| {
                        Endpoint::from(meta.addrs.target_addr())
                    }))
                    .into_inner(),
            );

            let proxy = Server::new(
                TransportLabels,
                metrics.transport,
                forward_tcp,
                server_stack.into_inner(),
                h2_settings,
                drain.clone(),
                disable_protocol_detection_for_ports.clone(),
            );

            let no_tls: tls::Conditional<identity::Local> =
                Conditional::None(tls::ReasonForNoPeerName::Loopback.into());
            let accept = tls::AcceptTls::new(no_tls, proxy)
                .with_skip_ports(disable_protocol_detection_for_ports);

            serve::serve(listen, accept, drain)
        }));

        Ok(Outbound { listen_addr, serve })
    }
}

#[derive(Copy, Clone, Debug)]
struct TransportLabels;

impl transport::metrics::TransportLabels<Endpoint> for TransportLabels {
    type Labels = transport::labels::Key;

    fn transport_labels(&self, endpoint: &Endpoint) -> Self::Labels {
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
