//! Maintains a connection to the Relayer.
//!
//! The external Relayer is responsible for the following:
//! - Acts as a TPU proxy.
//! - Sends transactions to the validator.
//! - Does not bundles to avoid DOS vector.
//! - When validator connects, it changes its TPU and TPU forward address to the relayer.
//! - Expected to send heartbeat to validator as watchdog. If watchdog times out, the validator
//!   disconnects and reverts the TPU and TPU forward settings.

use {
    crate::{
        proto_packet_to_packet,
        proxy::{
            auth::{generate_auth_tokens, maybe_refresh_auth_tokens, AuthInterceptor},
            HeartbeatEvent, ProxyError,
        },
        sigverify::SigverifyTracerPacketStats,
    },
    crossbeam_channel::Sender,
    jito_protos::proto::{
        auth::{auth_service_client::AuthServiceClient, Token},
        relayer::{self, relayer_client::RelayerClient},
    },
    solana_gossip::cluster_info::ClusterInfo,
    solana_perf::packet::PacketBatch,
    solana_sdk::{
        saturating_add_assign,
        signature::{Keypair, Signer},
    },
    std::{
        net::{IpAddr, Ipv4Addr, SocketAddr},
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc, Mutex,
        },
        thread::{self, Builder, JoinHandle},
        time::{Duration, Instant},
    },
    tokio::time::{interval, sleep, timeout},
    tonic::{
        codegen::InterceptedService,
        transport::{Channel, Endpoint},
        Streaming,
    },
};

const CONNECTION_TIMEOUT_S: u64 = 10;
const CONNECTION_BACKOFF_S: u64 = 5;

#[derive(Default)]
struct RelayerStageStats {
    num_empty_messages: u64,
    num_packets: u64,
    num_heartbeats: u64,
}

impl RelayerStageStats {
    pub(crate) fn report(&self) {
        datapoint_info!(
            "relayer_stage-stats",
            ("num_empty_messages", self.num_empty_messages, i64),
            ("num_packets", self.num_packets, i64),
            ("num_heartbeats", self.num_heartbeats, i64),
        );
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RelayerConfig {
    /// Auth Service Address
    pub auth_service_addr: String,

    /// Block Engine Address
    pub backend_addr: String,

    /// Interval at which heartbeats are expected.
    pub expected_heartbeat_interval: Duration,

    /// The max tolerable age of the last heartbeat.
    pub oldest_allowed_heartbeat: Duration,

    /// If set then it will be assumed the backend verified packets so signature verification will be bypassed in the validator.
    pub trust_packets: bool,
}

pub struct RelayerStage {
    t_hdls: Vec<JoinHandle<()>>,
}

impl RelayerStage {
    pub fn new(
        relayer_config: Arc<Mutex<RelayerConfig>>,
        // The keypair stored here is used to sign auth challenges.
        cluster_info: Arc<ClusterInfo>,
        // Channel that server-sent heartbeats are piped through.
        heartbeat_tx: Sender<HeartbeatEvent>,
        // Channel that non-trusted streamed packets are piped through.
        packet_tx: Sender<PacketBatch>,
        // Channel that trusted streamed packets are piped through.
        verified_packet_tx: Sender<(Vec<PacketBatch>, Option<SigverifyTracerPacketStats>)>,
        exit: Arc<AtomicBool>,
    ) -> Self {
        let thread = Builder::new()
            .name("relayer-stage".to_string())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                    .unwrap();

                rt.block_on(Self::start(
                    relayer_config,
                    cluster_info,
                    heartbeat_tx,
                    packet_tx,
                    verified_packet_tx,
                    exit,
                ));
            })
            .unwrap();

        Self {
            t_hdls: vec![thread],
        }
    }

    pub fn join(self) -> thread::Result<()> {
        for t in self.t_hdls {
            t.join()?;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn start(
        relayer_config: Arc<Mutex<RelayerConfig>>,
        cluster_info: Arc<ClusterInfo>,
        heartbeat_tx: Sender<HeartbeatEvent>,
        packet_tx: Sender<PacketBatch>,
        verified_packet_tx: Sender<(Vec<PacketBatch>, Option<SigverifyTracerPacketStats>)>,
        exit: Arc<AtomicBool>,
    ) {
        const CONNECTION_TIMEOUT: Duration = Duration::from_secs(CONNECTION_TIMEOUT_S);
        const CONNECTION_BACKOFF: Duration = Duration::from_secs(CONNECTION_BACKOFF_S);

        let mut error_count: u64 = 0;

        while !exit.load(Ordering::Relaxed) {
            // Wait until a valid config is supplied (either initially or by admin rpc)
            // Use if!/else here to avoid extra CONNECTION_BACKOFF wait on successful termination
            if !Self::validate_relayer_config(&relayer_config.lock().unwrap()) {
                sleep(CONNECTION_BACKOFF).await;
            } else if let Err(e) = Self::connect_auth_and_stream(
                &relayer_config,
                &cluster_info,
                &heartbeat_tx,
                &packet_tx,
                &verified_packet_tx,
                &exit,
                &CONNECTION_TIMEOUT,
            )
            .await
            {
                match e {
                    // This error is frequent on hot spares, and the parsed string does not work
                    // with datapoints (incorrect escaping).
                    ProxyError::AuthenticationPermissionDenied => {
                        warn!("block engine permission denied. not on leader schedule. ignore if hot-spare.")
                    }
                    e => {
                        error_count += 1;
                        datapoint_warn!(
                            "relayer_stage-proxy_error",
                            ("count", error_count, i64),
                            ("error", e.to_string(), String),
                        );
                    }
                }
                sleep(CONNECTION_BACKOFF).await;
            }
        }
    }

    async fn connect_auth_and_stream(
        relayer_config: &Arc<Mutex<RelayerConfig>>,
        cluster_info: &Arc<ClusterInfo>,
        heartbeat_tx: &Sender<HeartbeatEvent>,
        packet_tx: &Sender<PacketBatch>,
        verified_packet_tx: &Sender<(Vec<PacketBatch>, Option<SigverifyTracerPacketStats>)>,
        exit: &Arc<AtomicBool>,
        connection_timeout: &Duration,
    ) -> crate::proxy::Result<()> {
        // Get a copy of configs here in case they have changed at runtime
        let keypair = cluster_info.keypair().clone();
        let local_config = relayer_config.lock().unwrap().clone();

        let mut auth_service_endpoint =
            Endpoint::from_shared(local_config.auth_service_addr.clone()).map_err(|_| {
                ProxyError::AuthenticationConnectionError(format!(
                    "invalid relayer url value: {}",
                    local_config.auth_service_addr
                ))
            })?;
        if local_config.auth_service_addr.contains("https") {
            auth_service_endpoint = auth_service_endpoint
                .tls_config(tonic::transport::ClientTlsConfig::new())
                .map_err(|_| {
                    ProxyError::AuthenticationConnectionError(
                        "failed to set tls_config for relayer auth service".to_string(),
                    )
                })?;
        }
        let mut backend_endpoint = Endpoint::from_shared(local_config.backend_addr.clone())
            .map_err(|_| {
                ProxyError::RelayerConnectionError(format!(
                    "invalid relayer url value: {}",
                    local_config.backend_addr
                ))
            })?
            .tcp_keepalive(Some(Duration::from_secs(60)));
        if local_config.backend_addr.contains("https") {
            backend_endpoint = backend_endpoint
                .tls_config(tonic::transport::ClientTlsConfig::new())
                .map_err(|_| {
                    ProxyError::RelayerConnectionError(
                        "failed to set tls_config for relayer service".to_string(),
                    )
                })?;
        }

        debug!("connecting to auth: {:?}", local_config.auth_service_addr);
        let auth_channel = timeout(*connection_timeout, auth_service_endpoint.connect())
            .await
            .map_err(|_| ProxyError::AuthenticationConnectionTimeout)?
            .map_err(|e| ProxyError::AuthenticationConnectionError(e.to_string()))?;

        let mut auth_client = AuthServiceClient::new(auth_channel);

        debug!("generating authentication token");
        let (access_token, refresh_token) = timeout(
            *connection_timeout,
            generate_auth_tokens(&mut auth_client, &keypair),
        )
        .await
        .map_err(|_| ProxyError::AuthenticationTimeout)??;

        datapoint_info!(
            "relayer_stage-tokens_generated",
            ("url", local_config.auth_service_addr, String),
            ("count", 1, i64),
        );

        debug!("connecting to relayer: {:?}", local_config.backend_addr);
        let relayer_channel = timeout(*connection_timeout, backend_endpoint.connect())
            .await
            .map_err(|_| ProxyError::RelayerConnectionTimeout)?
            .map_err(|e| ProxyError::RelayerConnectionError(e.to_string()))?;

        let access_token = Arc::new(Mutex::new(access_token));
        let relayer_client = RelayerClient::with_interceptor(
            relayer_channel,
            AuthInterceptor::new(access_token.clone()),
        );

        Self::start_consuming_relayer_packets(
            relayer_client,
            heartbeat_tx,
            packet_tx,
            verified_packet_tx,
            &local_config,
            relayer_config,
            exit,
            auth_client,
            access_token,
            refresh_token,
            keypair,
            cluster_info,
            connection_timeout,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn start_consuming_relayer_packets(
        mut client: RelayerClient<InterceptedService<Channel, AuthInterceptor>>,
        heartbeat_tx: &Sender<HeartbeatEvent>,
        packet_tx: &Sender<PacketBatch>,
        verified_packet_tx: &Sender<(Vec<PacketBatch>, Option<SigverifyTracerPacketStats>)>,
        local_config: &RelayerConfig,
        global_config: &Arc<Mutex<RelayerConfig>>,
        exit: &Arc<AtomicBool>,
        auth_client: AuthServiceClient<Channel>,
        access_token: Arc<Mutex<Token>>,
        refresh_token: Token,
        keypair: Arc<Keypair>,
        cluster_info: &Arc<ClusterInfo>,
        connection_timeout: &Duration,
    ) -> crate::proxy::Result<()> {
        let heartbeat_event: HeartbeatEvent = {
            let tpu_config = timeout(
                *connection_timeout,
                client.get_tpu_configs(relayer::GetTpuConfigsRequest {}),
            )
            .await
            .map_err(|_| ProxyError::MethodTimeout("relayer_get_tpu_configs".to_string()))?
            .map_err(|e| ProxyError::MethodError(e.to_string()))?
            .into_inner();

            let tpu_addr = tpu_config
                .tpu
                .ok_or_else(|| ProxyError::MissingTpuSocket("tpu".to_string()))?;
            let tpu_forward_addr = tpu_config
                .tpu_forward
                .ok_or_else(|| ProxyError::MissingTpuSocket("tpu_fwd".to_string()))?;

            let tpu_ip = IpAddr::from(tpu_addr.ip.parse::<Ipv4Addr>()?);
            let tpu_forward_ip = IpAddr::from(tpu_forward_addr.ip.parse::<Ipv4Addr>()?);

            let tpu_socket = SocketAddr::new(tpu_ip, tpu_addr.port as u16);
            let tpu_forward_socket = SocketAddr::new(tpu_forward_ip, tpu_forward_addr.port as u16);
            (tpu_socket, tpu_forward_socket)
        };

        let packet_stream = timeout(
            *connection_timeout,
            client.subscribe_packets(relayer::SubscribePacketsRequest {}),
        )
        .await
        .map_err(|_| ProxyError::MethodTimeout("relayer_subscribe_packets".to_string()))?
        .map_err(|e| ProxyError::MethodError(e.to_string()))?
        .into_inner();

        Self::consume_packet_stream(
            heartbeat_event,
            heartbeat_tx,
            packet_stream,
            packet_tx,
            local_config,
            global_config,
            verified_packet_tx,
            exit,
            auth_client,
            access_token,
            refresh_token,
            keypair,
            cluster_info,
            connection_timeout,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn consume_packet_stream(
        heartbeat_event: HeartbeatEvent,
        heartbeat_tx: &Sender<HeartbeatEvent>,
        mut packet_stream: Streaming<relayer::SubscribePacketsResponse>,
        packet_tx: &Sender<PacketBatch>,
        local_config: &RelayerConfig,
        global_config: &Arc<Mutex<RelayerConfig>>,
        verified_packet_tx: &Sender<(Vec<PacketBatch>, Option<SigverifyTracerPacketStats>)>,
        exit: &Arc<AtomicBool>,
        mut auth_client: AuthServiceClient<Channel>,
        access_token: Arc<Mutex<Token>>,
        mut refresh_token: Token,
        keypair: Arc<Keypair>,
        cluster_info: &Arc<ClusterInfo>,
        connection_timeout: &Duration,
    ) -> crate::proxy::Result<()> {
        const METRICS_TICK: Duration = Duration::from_secs(1);
        let refresh_within_s: u64 = METRICS_TICK.as_secs().saturating_mul(3).saturating_div(2);

        let mut relayer_stats = RelayerStageStats::default();
        let mut metrics_and_auth_tick = interval(METRICS_TICK);

        let mut num_full_refreshes: u64 = 1;
        let mut num_refresh_access_token: u64 = 0;

        let mut heartbeat_check_interval = interval(local_config.expected_heartbeat_interval);
        let mut last_heartbeat_ts = Instant::now();

        info!("connected to packet stream");

        while !exit.load(Ordering::Relaxed) {
            tokio::select! {
                maybe_msg = packet_stream.message() => {
                    let resp = maybe_msg?.ok_or(ProxyError::GrpcStreamDisconnected)?;
                    Self::handle_relayer_packets(resp, heartbeat_event, heartbeat_tx, &mut last_heartbeat_ts, packet_tx, local_config.trust_packets, verified_packet_tx, &mut relayer_stats)?;
                }
                _ = heartbeat_check_interval.tick() => {
                    if last_heartbeat_ts.elapsed() > local_config.oldest_allowed_heartbeat {
                        return Err(ProxyError::HeartbeatExpired);
                    }
                }
                _ = metrics_and_auth_tick.tick() => {
                    relayer_stats.report();
                    relayer_stats = RelayerStageStats::default();

                    if cluster_info.id() != keypair.pubkey() {
                        return Err(ProxyError::AuthenticationConnectionError("validator identity changed".to_string()));
                    }

                    if *global_config.lock().unwrap() != *local_config {
                        return Err(ProxyError::AuthenticationConnectionError("relayer config changed".to_string()));
                    }

                    let (maybe_new_access, maybe_new_refresh) = maybe_refresh_auth_tokens(&mut auth_client,
                        &access_token,
                        &refresh_token,
                        cluster_info,
                        connection_timeout,
                        refresh_within_s,
                    ).await?;

                    if let Some(new_token) = maybe_new_access {
                        num_refresh_access_token += 1;
                        datapoint_info!(
                            "relayer_stage-refresh_access_token",
                            ("url", &local_config.auth_service_addr, String),
                            ("count", num_refresh_access_token, i64),
                        );
                        *access_token.lock().unwrap() = new_token;
                    }
                    if let Some(new_token) = maybe_new_refresh {
                        num_full_refreshes += 1;
                        datapoint_info!(
                            "relayer_stage-tokens_generated",
                            ("url", &local_config.auth_service_addr, String),
                            ("count", num_full_refreshes, i64),
                        );
                        refresh_token = new_token;
                    }
                }
            }
        }
        Ok(())
    }

    fn handle_relayer_packets(
        subscribe_packets_resp: relayer::SubscribePacketsResponse,
        heartbeat_event: HeartbeatEvent,
        heartbeat_tx: &Sender<HeartbeatEvent>,
        last_heartbeat_ts: &mut Instant,
        packet_tx: &Sender<PacketBatch>,
        trust_packets: bool,
        verified_packet_tx: &Sender<(Vec<PacketBatch>, Option<SigverifyTracerPacketStats>)>,
        relayer_stats: &mut RelayerStageStats,
    ) -> crate::proxy::Result<()> {
        match subscribe_packets_resp.msg {
            None => {
                saturating_add_assign!(relayer_stats.num_empty_messages, 1);
            }
            Some(relayer::subscribe_packets_response::Msg::Batch(proto_batch)) => {
                if proto_batch.packets.is_empty() {
                    saturating_add_assign!(relayer_stats.num_empty_messages, 1);
                    return Ok(());
                }

                let packet_batch = PacketBatch::new(
                    proto_batch
                        .packets
                        .into_iter()
                        .map(proto_packet_to_packet)
                        .collect(),
                );

                saturating_add_assign!(relayer_stats.num_packets, packet_batch.len() as u64);

                if trust_packets {
                    verified_packet_tx
                        .send((vec![packet_batch], None))
                        .map_err(|_| ProxyError::PacketForwardError)?;
                } else {
                    packet_tx
                        .send(packet_batch)
                        .map_err(|_| ProxyError::PacketForwardError)?;
                }
            }
            Some(relayer::subscribe_packets_response::Msg::Heartbeat(_)) => {
                saturating_add_assign!(relayer_stats.num_heartbeats, 1);

                *last_heartbeat_ts = Instant::now();
                heartbeat_tx
                    .send(heartbeat_event)
                    .map_err(|_| ProxyError::HeartbeatChannelError)?;
            }
        }
        Ok(())
    }

    fn validate_relayer_config(config: &RelayerConfig) -> bool {
        if config.auth_service_addr.is_empty() {
            warn!("Can't connect to relayer auth. Missing or invalid url.");
            return false;
        }
        if config.backend_addr.is_empty() {
            warn!("Can't connect to relayer. Missing or invalid url.");
            return false;
        }
        if config.oldest_allowed_heartbeat.is_zero() {
            warn!("Relayer oldest allowed heartbeat must be greater than 0.");
            return false;
        }
        if config.expected_heartbeat_interval.is_zero() {
            warn!("Relayer expected heartbeat interval must be greater than 0.");
            return false;
        }
        true
    }
}
