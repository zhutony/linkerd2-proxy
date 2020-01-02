//! Configures and runs the inbound proxy.
//!
//! The inbound proxy is responsible for terminating traffic from other network
//! endpoints inbound to the local application.

//#![deny(warnings, rust_2018_idioms)]
#![allow(warnings, rust_2018_idioms)]

pub use self::endpoint::{Endpoint, Profile, ProfileTarget, RequestTarget, Target};
use futures::future;
use linkerd2_app_core::{
    self as core, cache, classify,
    config::{ProxyConfig, ServerConfig},
    drain, dst, errors,
    opencensus::proto::trace::v1 as oc,
    proxy::{
        self,
        http::{
            client, insert, metrics as http_metrics, normalize_uri, orig_proto, profiles,
            strip_header,
        },
        identity,
        server::{Protocol as ServerProtocol, Server},
        tap, tcp,
    },
    reconnect, router, serve,
    spans::SpanConverter,
    svc::{self, Make},
    trace, trace_context,
    transport::{self, connect, tls, OrigDstAddr, SysOrigDstAddr},
    Addr, DispatchDeadline, Error, ProxyMetrics, DST_OVERRIDE_HEADER, L5D_CLIENT_ID, L5D_REMOTE_IP,
    L5D_SERVER_ID,
};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::Duration;
use tokio::sync::mpsc;
use tower_grpc::{self as grpc, generic::client::GrpcService};
use tracing::{info, info_span};

mod endpoint;
#[allow(dead_code)] // TODO #2597
mod set_client_id_on_req;
#[allow(dead_code)] // TODO #2597
mod set_remote_ip_on_req;

#[derive(Clone, Debug)]
pub struct Config<A: OrigDstAddr = SysOrigDstAddr> {
    pub proxy: ProxyConfig<A>,
}

pub struct Inbound {
    pub listen_addr: SocketAddr,
    pub serve: serve::Task,
}

impl<A: OrigDstAddr> Config<A> {
    pub fn with_orig_dst_addr<B: OrigDstAddr>(self, orig_dst_addr: B) -> Config<B> {
        Config {
            proxy: self.proxy.with_orig_dst_addr(orig_dst_addr),
        }
    }

    pub fn build<P>(
        self,
        local_identity: tls::Conditional<identity::Local>,
        profiles_client: P,
        tap_layer: tap::Layer,
        metrics: ProxyMetrics,
        span_sink: Option<mpsc::Sender<oc::Span>>,
        drain: drain::Watch,
    ) -> Result<Inbound, Error>
    where
        A: Send + 'static,
        P: profiles::GetRoutes<Profile> + Clone + Send + 'static,
        P::Future: Send,
    {
        use proxy::core::listen::{Bind, Listen};
        let Config {
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
            // Establishes connections to the local application (for both
            // TCP forwarding and HTTP proxying).
            let connect_stack = svc::stack(connect::svc(connect.keepalive))
                .push_timeout(connect.timeout)
                .push(metrics.transport.layer_connect(TransportLabels));

            let endpoint_cache = connect_stack
                .clone()
                .push(client::layer(connect.h2_settings))
                .push(reconnect::layer({
                    let backoff = connect.backoff.clone();
                    move |_| Ok(backoff.stream())
                }))
                .push_per_make(trace_context::layer(
                    span_sink
                        .clone()
                        .map(|span_sink| SpanConverter::client(span_sink, trace_labels())),
                ))
                .serves::<Endpoint>()
                .push_pending()
                .push_per_make(svc::lock::Layer::new())
                .makes::<Endpoint>()
                .spawn_cache(router_capacity, router_max_idle_age)
                .push(trace::layer(|ep: &Endpoint| {
                    info_span!(
                        "endpoint",
                        port = %ep.port,
                        http = ?ep.settings,
                    )
                }));

            // Determine the target for each request, obtain a profile route for
            // that target, and dispatch the request to it.
            let profile_cache = svc::stack(endpoint_cache)
                .serves::<Endpoint>()
                .push(svc::map_target::layer(Endpoint::from))
                .push(normalize_uri::layer())
                .push(tap_layer)
                .push(http_metrics::layer::<_, classify::Response>(
                    metrics.http_endpoint,
                ))
                .serves::<Target>()
                .push(profiles::Layer::without_overrides(
                    profiles_client,
                    svc::stack(())
                        .push(insert::target::layer())
                        .push(http_metrics::layer::<_, classify::Response>(
                            metrics.http_route,
                        ))
                        .push(classify::layer())
                        .makes_clone::<dst::Route>()
                        .into_inner(),
                ))
                .push_pending()
                .push_per_make(svc::lock::Layer::new())
                .makes_clone::<Profile>()
                .spawn_cache(router_capacity, router_max_idle_age)
                .push(trace::layer(|_: &Profile| info_span!("profile")))
                .serves::<Profile>()
                .push(router::Layer::new(|()| ProfileTarget))
                .make(());

            // Determine the target for each request, obtain a profile route for
            // that target, and dispatch the request to it.
            let source_stack = svc::stack(profile_cache)
                .serves::<Target>()
                .push_make_ready()
                .push_per_make(
                    svc::layers()
                        .push_ready_timeout(Duration::from_secs(10))
                        .push(strip_header::request::layer(DST_OVERRIDE_HEADER)),
                )
                .push(trace::layer(|t: &Target| {
                    info_span!(
                        "target",
                        name = ?t.dst_name,
                        http = ?t.http_settings,
                    )
                }))
                .push(router::Layer::new(RequestTarget::from))
                .makes::<tls::accept::Meta>()
                .push(trace::layer(|src: &tls::accept::Meta| info_span!("router")))
                .push_per_make(
                    svc::layers()
                        .push(svc::layer::mk(orig_proto::Downgrade::new))
                        .push(strip_header::request::layer(L5D_REMOTE_IP))
                        .push(strip_header::request::layer(L5D_CLIENT_ID))
                        .push(strip_header::response::layer(L5D_SERVER_ID))
                        .push_concurrency_limit(buffer.max_in_flight)
                        .push_load_shed()
                        .push(errors::layer())
                        .push(trace_context::layer(span_sink.map(|span_sink| {
                            SpanConverter::server(span_sink, trace_labels())
                        })))
                        .push(metrics.http_handle_time.layer())
                        .push_oneshot(),
                )
                .push(trace::layer(|src: &tls::accept::Meta| {
                    info_span!(
                        "source",
                        peer.id = ?src.peer_identity,
                        target.addr = %src.addrs.target_addr(),
                    )
                }));

            let forward_tcp = tcp::Forward::new(
                svc::stack(connect_stack)
                    .push(svc::map_target::layer(|meta: tls::accept::Meta| {
                        Endpoint::from(meta.addrs.target_addr())
                    }))
                    .into_inner(),
            );

            let server = Server::new(
                TransportLabels,
                metrics.transport,
                forward_tcp,
                source_stack.into_inner(),
                h2_settings,
                drain.clone(),
                disable_protocol_detection_for_ports.clone(),
            );

            let accept = tls::AcceptTls::new(local_identity, server)
                .with_skip_ports(disable_protocol_detection_for_ports);

            info!(listen.addr = %listen.listen_addr(), "serving");
            serve::serve(listen, accept, drain)
        }));

        Ok(Inbound { listen_addr, serve })
    }
}

#[derive(Copy, Clone, Debug)]
struct TransportLabels;

impl transport::metrics::TransportLabels<Endpoint> for TransportLabels {
    type Labels = transport::labels::Key;

    fn transport_labels(&self, _: &Endpoint) -> Self::Labels {
        transport::labels::Key::connect::<()>(
            "inbound",
            tls::Conditional::None(tls::ReasonForNoPeerName::Loopback.into()),
        )
    }
}

impl transport::metrics::TransportLabels<ServerProtocol> for TransportLabels {
    type Labels = transport::labels::Key;

    fn transport_labels(&self, proto: &ServerProtocol) -> Self::Labels {
        transport::labels::Key::accept("inbound", proto.tls.peer_identity.as_ref())
    }
}

pub fn trace_labels() -> HashMap<String, String> {
    let mut l = HashMap::new();
    l.insert("direction".to_string(), "inbound".to_string());
    l
}
