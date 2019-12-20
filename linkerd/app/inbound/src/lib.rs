//! Configures and runs the inbound proxy.
//!
//! The inbound proxy is responsible for terminating traffic from other network
//! endpoints inbound to the local application.

#![deny(warnings, rust_2018_idioms)]

pub use self::endpoint::{Endpoint, RecognizeTarget, Target};
use futures::future;
use linkerd2_app_core::{
    self as core, cache, classify,
    config::{ProxyConfig, ServerConfig},
    drain, dst, errors,
    opencensus::proto::trace::v1 as oc,
    proxy::{
        self,
        http::{client, insert, metrics as http_metrics, normalize_uri, profiles, strip_header},
        identity,
        server::{Protocol as ServerProtocol, Server},
        tap, tcp,
    },
    reconnect, router, serve,
    spans::SpanConverter,
    svc, trace, trace_context,
    transport::{self, connect, tls, OrigDstAddr, SysOrigDstAddr},
    DispatchDeadline, Error, ProxyMetrics, DST_OVERRIDE_HEADER, L5D_CLIENT_ID, L5D_REMOTE_IP,
    L5D_SERVER_ID,
};
use std::collections::HashMap;
use std::net::SocketAddr;
use tokio::sync::mpsc;
use tower_grpc::{self as grpc, generic::client::GrpcService};
use tracing::{info, info_span};

mod endpoint;
mod orig_proto_downgrade;
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
        profiles_client: core::profiles::Client<P>,
        tap_layer: tap::Layer,
        metrics: ProxyMetrics,
        span_sink: Option<mpsc::Sender<oc::Span>>,
        drain: drain::Watch,
    ) -> Result<Inbound, Error>
    where
        A: Send + 'static,
        P: GrpcService<grpc::BoxBody> + Clone + Send + Sync + 'static,
        P::ResponseBody: Send,
        <P::ResponseBody as grpc::Body>::Data: Send,
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
                .push_pending()
                .push_buffer(2, DispatchDeadline::extract)
                .push(cache::Layer::new(router_capacity, router_max_idle_age))
                .into_inner()
                .spawn();

            // let admission_control = svc::stack(dst_router)
            //     .push_concurrency_limit(buffer.max_in_flight)
            //     .push_load_shed()
            //     .into_inner();

            let profile_cache = svc::stack(endpoint_cache)
                .serves::<Endpoint>()
                .push(svc::map_target::layer(Endpoint::from))
                .push(trace_context::layer(span_sink.clone().map(|span_sink| {
                    SpanConverter::client(span_sink, trace_labels())
                })))
                .push(normalize_uri::layer())
                .push(tap_layer)
                .push(http_metrics::layer::<_, classify::Response>(
                    metrics.http_endpoint,
                ))
                .push(profiles::Layer::without_split(
                    profiles_client,
                    svc::stack(())
                        .makes::<dst::Route>()
                        .push(http_metrics::layer::<_, classify::Response>(
                            metrics.http_route,
                        ))
                        .makes::<dst::Route>()
                        .push(classify::layer())
                        .push(insert::target::layer())
                        .into_inner(),
                ))
                .push_pending()
                .push_buffer(2, DispatchDeadline::extract)
                .push(cache::Layer::new(router_capacity, router_max_idle_age))
                .serves::<Target>()
                .into_inner()
                .spawn();

            let target_stack = svc::stack(profile_cache).push(router::Layer::new(ProfileKey::from));

            let source_stack = target_stack
                .push(router::Layer::new(RecognizeTarget::new))
                .makes::<tls::accept::Meta>()
                .into_inner();

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
                source_stack,
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
