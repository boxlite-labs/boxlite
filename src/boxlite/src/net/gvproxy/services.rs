//! The core-side gvproxy backend: produces the shim provisioning spec and drives
//! gvproxy's ServicesMux (runtime network control).
//!
//! [`GvproxyBackend`] is the [`NetworkBackend`] the boxlite *core* owns for a box
//! (see `BoxImpl::network`). From its held [`NetworkBackendConfig`] it produces
//! the wire [`NetworkBackendSpec`] (`spec()`), and for a running box it dials
//! gvproxy's control socket (`gvproxy-ctl.sock`) over HTTP/1.1-on-unix to drive
//! dynamic port forwarding, DNS, and lease/stat inspection — mirroring the
//! unix-connector pattern in `portal/connection.rs`.
//!
//! The socket is bound by gvproxy inside the shim (see the Go bridge) and lives
//! for the VM's lifetime, so a core process that reconnects after detach can
//! still change forwards.

use std::net::{SocketAddr, TcpListener, UdpSocket};
use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use boxlite_shared::errors::{BoxliteError, BoxliteResult};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::client::conn::http1;
use hyper::{Method, Request};
use hyper_util::rt::TokioIo;
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

use crate::net::{
    BoxInternalTunnel, DnsZoneSpec, Forward, NetworkBackend, NetworkBackendConfig,
    NetworkBackendSpec, NetworkBackendStats, TransportProtocol,
};

/// Upper bound on a single control exchange. A bound-but-unserved socket (the
/// tiny window between `listen` and `serve` in the shim) must never hang a call.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Maximum number of kernel-selected ports tried for one automatic forward.
const EXPOSE_AUTO_PORT_ATTEMPTS: usize = 5;

const EXPOSE_PATH: &str = "/services/forwarder/expose";

#[derive(Debug)]
enum ExposeResponse {
    Published,
    BindConflict { status: u16, body: String },
}

enum HostPortReservation {
    Tcp(TcpListener),
    Udp(UdpSocket),
}

impl HostPortReservation {
    fn bind(requested: SocketAddr, protocol: TransportProtocol) -> BoxliteResult<Self> {
        match protocol {
            TransportProtocol::Tcp => TcpListener::bind(requested).map(Self::Tcp),
            TransportProtocol::Udp => UdpSocket::bind(requested).map(Self::Udp),
            TransportProtocol::Unix | TransportProtocol::Npipe => {
                return Err(BoxliteError::Unsupported(format!(
                    "automatic host port allocation is not supported for {} forwards",
                    protocol.as_str()
                )));
            }
        }
        .map_err(|error| {
            BoxliteError::Network(format!(
                "preselect {} host port for gvproxy expose {requested}: {error}",
                protocol.as_str()
            ))
        })
    }

    fn local_addr(&self) -> std::io::Result<SocketAddr> {
        match self {
            Self::Tcp(listener) => listener.local_addr(),
            Self::Udp(socket) => socket.local_addr(),
        }
    }
}

/// gvproxy backend — the [`NetworkBackend`] the core constructs for a box from
/// its [`NetworkBackendConfig`]. It produces the wire [`NetworkBackendSpec`] (via
/// [`spec`](NetworkBackend::spec)) and is the runtime control client: each
/// control call opens a one-shot HTTP/1.1 connection to the derived control socket.
#[derive(Debug, Clone)]
pub struct GvproxyBackend {
    /// The box's network config — read by [`spec`](NetworkBackend::spec) and the
    /// source of the data socket the control socket is derived from.
    config: NetworkBackendConfig,
    /// gvproxy's control socket (`gvproxy-ctl.sock`) — dialed for control.
    control_socket_path: PathBuf,
}

impl GvproxyBackend {
    /// Build the gvproxy backend for the box described by `config`. The control
    /// socket is derived as a sibling of the data socket
    /// ([`super::control_socket_path`]), so no gvproxy-specific socket path leaks
    /// into neutral layers.
    pub fn from_config(config: &NetworkBackendConfig) -> Self {
        Self {
            control_socket_path: super::control_socket_path(&config.socket_path),
            config: config.clone(),
        }
    }

    /// One-shot HTTP/1.1 request to the services socket. Returns `(status, body)`.
    async fn request(
        &self,
        method: Method,
        path: &str,
        body: Option<String>,
    ) -> BoxliteResult<(u16, String)> {
        let exchange = async move {
            let stream = UnixStream::connect(&self.control_socket_path)
                .await
                .map_err(|e| {
                    BoxliteError::Network(format!(
                        "gvproxy services connect {} failed: {e}",
                        self.control_socket_path.display()
                    ))
                })?;

            let (mut sender, conn) = http1::handshake(TokioIo::new(stream)).await.map_err(|e| {
                BoxliteError::Network(format!("gvproxy services handshake failed: {e}"))
            })?;

            // Drive the connection concurrently while we read the response.
            tokio::spawn(async move {
                let _ = conn.await;
            });

            let req = Request::builder()
                .method(method)
                .uri(path)
                .header(hyper::header::HOST, "gvproxy")
                .body(Full::<Bytes>::new(Bytes::from(body.unwrap_or_default())))
                .map_err(|e| {
                    BoxliteError::Network(format!("gvproxy services request build failed: {e}"))
                })?;

            let resp = sender.send_request(req).await.map_err(|e| {
                BoxliteError::Network(format!("gvproxy services request to {path} failed: {e}"))
            })?;

            let status = resp.status().as_u16();
            let bytes = resp
                .into_body()
                .collect()
                .await
                .map_err(|e| {
                    BoxliteError::Network(format!("gvproxy services read {path} failed: {e}"))
                })?
                .to_bytes();
            Ok((status, String::from_utf8_lossy(&bytes).into_owned()))
        };

        // Explicit timeout for external work (CLAUDE.md): never hang a control call.
        tokio::time::timeout(REQUEST_TIMEOUT, exchange)
            .await
            .map_err(|_| {
                BoxliteError::Network(format!(
                    "gvproxy services request to {path} timed out after {}s",
                    REQUEST_TIMEOUT.as_secs()
                ))
            })?
    }

    /// Send a request and require a 2xx, returning the body. A non-2xx carries
    /// gvproxy's own message (e.g. `"proxy already running"`, `"proxy not found"`).
    async fn request_ok(
        &self,
        method: Method,
        path: &str,
        body: Option<String>,
    ) -> BoxliteResult<String> {
        let (status, body) = self.request(method, path, body).await?;
        if (200..300).contains(&status) {
            Ok(body)
        } else {
            Err(BoxliteError::Network(format!(
                "gvproxy {path} returned {status}: {}",
                body.trim()
            )))
        }
    }

    /// Send one expose request while retaining enough of the raw response to
    /// distinguish gvproxy's known bind-conflict replies from other failures.
    async fn request_expose(
        &self,
        local: &str,
        remote: &str,
        protocol: TransportProtocol,
    ) -> BoxliteResult<ExposeResponse> {
        let body = serde_json::json!({
            "local": local,
            "remote": remote,
            "protocol": protocol.as_str(),
        });
        let (status, response_body) = self
            .request(Method::POST, EXPOSE_PATH, Some(body.to_string()))
            .await?;

        if (200..300).contains(&status) {
            return Ok(ExposeResponse::Published);
        }
        if is_expose_bind_conflict(status, &response_body) {
            return Ok(ExposeResponse::BindConflict {
                status,
                body: response_body,
            });
        }

        Err(expose_response_error(status, &response_body))
    }

    async fn expose_fixed(
        &self,
        local: &str,
        remote: &str,
        protocol: TransportProtocol,
    ) -> BoxliteResult<Forward> {
        match self.request_expose(local, remote, protocol).await? {
            ExposeResponse::Published => Ok(Forward {
                local: local.to_string(),
                remote: remote.to_string(),
                protocol: protocol.as_str().to_string(),
            }),
            ExposeResponse::BindConflict { status, body } => Err(bind_conflict_error(
                &format!("gvproxy {EXPOSE_PATH}"),
                status,
                &body,
            )),
        }
    }

    /// Publish a forward on an OS-selected host port, retrying the handoff race
    /// a bounded number of times.
    ///
    /// The pinned gvproxy stores the literal `:0` address, `/expose` returns an
    /// empty body, and `/all` reports `:0` instead of the selected port — so a
    /// second automatic mapping collides on that same literal key. Preselecting
    /// a concrete kernel port keeps mappings distinct and lets callers discover
    /// the binding. The reservation has to be released before gvproxy binds,
    /// which is why a known bind-conflict response is retried rather than
    /// surfaced.
    async fn expose_auto(
        &self,
        requested: SocketAddr,
        remote: &str,
        protocol: TransportProtocol,
    ) -> BoxliteResult<Forward> {
        // The first attempt also establishes the conflict to report if every
        // retry loses the same race.
        let mut conflict = match self.expose_preselected(requested, remote, protocol).await? {
            Ok(forward) => return Ok(forward),
            Err(conflict) => conflict,
        };
        for _ in 1..EXPOSE_AUTO_PORT_ATTEMPTS {
            match self.expose_preselected(requested, remote, protocol).await? {
                Ok(forward) => return Ok(forward),
                Err(latest) => conflict = latest,
            }
        }

        let (status, body) = conflict;
        Err(bind_conflict_error(
            &format!(
                "gvproxy {EXPOSE_PATH} exhausted {EXPOSE_AUTO_PORT_ATTEMPTS} automatic \
                 port attempts; last response"
            ),
            status,
            &body,
        ))
    }

    /// One automatic-allocation attempt: reserve a kernel-selected port, release
    /// it, and ask gvproxy to bind that exact endpoint. `Err` carries gvproxy's
    /// bind-conflict response, which is retryable; a real failure short-circuits.
    async fn expose_preselected(
        &self,
        requested: SocketAddr,
        remote: &str,
        protocol: TransportProtocol,
    ) -> BoxliteResult<Result<Forward, (u16, String)>> {
        let reservation = HostPortReservation::bind(SocketAddr::new(requested.ip(), 0), protocol)?;
        let resolved_local = reservation.local_addr().map_err(|error| {
            BoxliteError::Network(format!(
                "read preselected {} host port for gvproxy expose {requested}: {error}",
                protocol.as_str()
            ))
        })?;
        let resolved_local = resolved_local.to_string();
        drop(reservation);

        Ok(
            match self
                .request_expose(&resolved_local, remote, protocol)
                .await?
            {
                ExposeResponse::Published => Ok(Forward {
                    local: resolved_local,
                    remote: remote.to_string(),
                    protocol: protocol.as_str().to_string(),
                }),
                ExposeResponse::BindConflict { status, body } => Err((status, body)),
            },
        )
    }
}

fn is_expose_bind_conflict(status: u16, body: &str) -> bool {
    status == 500
        && (body.trim() == "proxy already running" || body.contains("bind: address already in use"))
}

fn expose_response_error(status: u16, body: &str) -> BoxliteError {
    BoxliteError::Network(format!(
        "gvproxy {EXPOSE_PATH} returned {status}: {}",
        body.trim()
    ))
}

fn bind_conflict_error(context: &str, status: u16, body: &str) -> BoxliteError {
    BoxliteError::AlreadyExists(format!("{context} returned {status}: {}", body.trim()))
}

#[async_trait]
impl NetworkBackend for GvproxyBackend {
    fn name(&self) -> &'static str {
        "gvisor-tap-vsock"
    }

    fn spec(&self) -> NetworkBackendSpec {
        let cfg = &self.config;
        let mut spec = NetworkBackendSpec {
            socket_path: cfg.socket_path.clone(),
            allow_net: cfg.allow_net.clone(),
            secrets: cfg.secrets.clone(),
            ca_cert_pem: None,
            ca_key_pem: None,
            net_bandwidth: cfg.net_bandwidth,
        };

        // Mint the ephemeral MITM CA when secrets are configured. The cert+key
        // flow through the spec → GvproxyConfig → Go. On failure, drop the
        // secrets rather than run MITM injection without a CA.
        if !cfg.secrets.is_empty() {
            match crate::net::ca::load_or_generate(&cfg.ca_dir) {
                Ok(ca) => {
                    spec.ca_cert_pem = Some(ca.cert_pem);
                    spec.ca_key_pem = Some(ca.key_pem);
                }
                Err(e) => {
                    tracing::error!("MITM: CA setup failed, secrets disabled: {e}");
                    spec.secrets.clear();
                }
            }
        }

        spec
    }

    async fn expose(
        &self,
        local: &str,
        remote: &str,
        protocol: TransportProtocol,
    ) -> BoxliteResult<Forward> {
        match protocol {
            TransportProtocol::Tcp | TransportProtocol::Udp => {
                let requested = local.parse::<SocketAddr>().map_err(|error| {
                    BoxliteError::Config(format!(
                        "invalid {} local endpoint {local:?}; expected IP:port: {error}",
                        protocol.as_str().to_ascii_uppercase()
                    ))
                })?;
                if requested.port() == 0 {
                    self.expose_auto(requested, remote, protocol).await
                } else {
                    self.expose_fixed(local, remote, protocol).await
                }
            }
            TransportProtocol::Unix | TransportProtocol::Npipe => {
                self.expose_fixed(local, remote, protocol).await
            }
        }
    }

    async fn unexpose(&self, local: &str, protocol: TransportProtocol) -> BoxliteResult<()> {
        let body = serde_json::json!({ "local": local, "protocol": protocol.as_str() });
        self.request_ok(
            Method::POST,
            "/services/forwarder/unexpose",
            Some(body.to_string()),
        )
        .await?;
        Ok(())
    }

    async fn list_forwards(&self) -> BoxliteResult<Vec<Forward>> {
        let body = self
            .request_ok(Method::GET, "/services/forwarder/all", None)
            .await?;
        serde_json::from_str(&body).map_err(|e| {
            BoxliteError::Network(format!(
                "gvproxy /services/forwarder/all parse failed: {e} (body: {body})"
            ))
        })
    }

    async fn add_dns_zone(&self, zone: DnsZoneSpec) -> BoxliteResult<()> {
        let wire = dns_zone_to_wire(&zone);
        self.request_ok(Method::POST, "/services/dns/add", Some(wire.to_string()))
            .await?;
        Ok(())
    }

    async fn dns_zones(&self) -> BoxliteResult<Value> {
        let body = self
            .request_ok(Method::GET, "/services/dns/all", None)
            .await?;
        parse_json(&body, "/services/dns/all")
    }

    async fn dhcp_leases(&self) -> BoxliteResult<Value> {
        let body = self
            .request_ok(Method::GET, "/services/dhcp/leases", None)
            .await?;
        parse_json(&body, "/services/dhcp/leases")
    }

    async fn cam(&self) -> BoxliteResult<Value> {
        let body = self.request_ok(Method::GET, "/cam", None).await?;
        parse_json(&body, "/cam")
    }

    async fn stats(&self) -> BoxliteResult<NetworkBackendStats> {
        let body = self.request_ok(Method::GET, "/stats", None).await?;
        parse_stats(&body)
    }

    async fn tunnel(&self, target: SocketAddr) -> BoxliteResult<BoxInternalTunnel> {
        // `/tunnel` hijacks the HTTP connection — no HTTP response is returned —
        // so we speak it raw (not via hyper): send gvproxy's request, read its
        // literal "OK" ack, and the socket becomes a raw pipe to the guest target.
        // Mirrors gvproxy's own `transport.Tunnel` (`POST /tunnel?ip=&port=`).
        let ctl = self.control_socket_path.clone();
        let handshake = async {
            let mut stream = UnixStream::connect(&ctl).await.map_err(|e| {
                BoxliteError::Network(format!(
                    "gvproxy tunnel connect {} failed: {e}",
                    ctl.display()
                ))
            })?;
            let req = format!(
                "POST /tunnel?ip={}&port={} HTTP/1.1\r\nHost: gvproxy\r\n\r\n",
                target.ip(),
                target.port()
            );
            stream.write_all(req.as_bytes()).await.map_err(|e| {
                BoxliteError::Network(format!("gvproxy tunnel request failed: {e}"))
            })?;
            let mut ack = [0u8; 2];
            stream.read_exact(&mut ack).await.map_err(|e| {
                BoxliteError::Network(format!("gvproxy tunnel ack read failed: {e}"))
            })?;
            Ok::<_, BoxliteError>((stream, ack))
        };

        // Bound only the handshake; the returned tunnel itself is long-lived.
        let (stream, ack) = tokio::time::timeout(REQUEST_TIMEOUT, handshake)
            .await
            .map_err(|_| {
                BoxliteError::Network(format!(
                    "gvproxy tunnel to {target} timed out after {}s",
                    REQUEST_TIMEOUT.as_secs()
                ))
            })??;

        if &ack != b"OK" {
            return Err(BoxliteError::Network(format!(
                "gvproxy tunnel handshake: expected \"OK\", got {:?}",
                String::from_utf8_lossy(&ack)
            )));
        }

        Ok(BoxInternalTunnel::from_local(stream, target))
    }
}

/// Parse gvproxy's `/stats` body onto the neutral [`NetworkBackendStats`]'s typed
/// fields. Parsing goes via gvproxy's own `NetworkStats` wire shape (PascalCase),
/// so that gvproxy-specific format stays out of `net::`.
fn parse_stats(body: &str) -> BoxliteResult<NetworkBackendStats> {
    let wire = super::NetworkStats::from_json_str(body).map_err(|e| {
        BoxliteError::Network(format!("gvproxy /stats parse failed: {e} (body: {body})"))
    })?;
    Ok(NetworkBackendStats {
        bytes_sent: wire.bytes_sent,
        bytes_received: wire.bytes_received,
        tcp_established: wire.tcp.current_established,
        tcp_failed_connections: wire.tcp.failed_connection_attempts,
        tcp_retransmits: wire.tcp.retransmits,
        tcp_timeouts: wire.tcp.timeouts,
        tcp_forward_max_inflight_drop: wire.tcp.forward_max_inflight_drop,
    })
}

fn parse_json(body: &str, path: &str) -> BoxliteResult<Value> {
    serde_json::from_str(body).map_err(|e| {
        BoxliteError::Network(format!("gvproxy {path} parse failed: {e} (body: {body})"))
    })
}

/// Map a neutral [`DnsZoneSpec`] to gvproxy's `types.Zone` wire form.
///
/// gvproxy's `types.Zone` Go struct has **no json tags**, so the wire keys are
/// the Go field names: `Name`, `Records`, `DefaultIP`, and per-record
/// `Name`/`IP`. This differs from the snake_case create-path config; getting the
/// case wrong silently no-ops the zone add, so this mapping is the single source
/// of truth for the runtime `POST /services/dns/add` body.
fn dns_zone_to_wire(zone: &DnsZoneSpec) -> Value {
    let records: Vec<Value> = zone
        .records
        .iter()
        .map(|r| serde_json::json!({ "Name": r.name, "IP": r.ip }))
        .collect();
    let mut obj = serde_json::json!({ "Name": zone.name, "Records": records });
    if let Some(default_ip) = &zone.default_ip {
        obj["DefaultIP"] = Value::from(default_ip.clone());
    }
    obj
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::DnsRecordSpec;

    #[test]
    fn dns_zone_maps_to_capitalized_wire_keys() {
        let zone = DnsZoneSpec {
            name: "myapp.local.".to_string(),
            records: vec![DnsRecordSpec {
                name: "api".to_string(),
                ip: "192.168.127.10".to_string(),
            }],
            default_ip: Some("192.168.127.254".to_string()),
        };
        let wire = dns_zone_to_wire(&zone);
        // gvproxy's types.Zone has no json tags → Go field names.
        assert_eq!(wire["Name"], "myapp.local.");
        assert_eq!(wire["DefaultIP"], "192.168.127.254");
        assert_eq!(wire["Records"][0]["Name"], "api");
        assert_eq!(wire["Records"][0]["IP"], "192.168.127.10");
        // Guard against snake_case leaking (the footgun this mapping exists for).
        assert!(wire.get("name").is_none());
        assert!(wire.get("default_ip").is_none());
        assert!(wire["Records"][0].get("ip").is_none());
    }

    #[test]
    fn dns_zone_without_default_omits_defaultip() {
        let zone = DnsZoneSpec {
            name: "z.".to_string(),
            records: vec![],
            default_ip: None,
        };
        let wire = dns_zone_to_wire(&zone);
        assert!(wire.get("DefaultIP").is_none());
    }

    #[test]
    fn forward_deserializes_from_gvproxy_all_payload() {
        // Shape of GET /services/forwarder/all.
        let payload =
            r#"[{"local":"127.0.0.1:2222","remote":"192.168.127.2:22","protocol":"tcp"}]"#;
        let forwards: Vec<Forward> = serde_json::from_str(payload).unwrap();
        assert_eq!(forwards.len(), 1);
        assert_eq!(forwards[0].local, "127.0.0.1:2222");
        assert_eq!(forwards[0].remote, "192.168.127.2:22");
        assert_eq!(forwards[0].protocol, "tcp");
    }

    #[test]
    fn spec_reflects_config_and_mints_no_ca_without_secrets() {
        // `spec()` turns the held config into the wire spec. Without secrets it
        // copies the fields through and mints no CA (never touching `ca_dir`).
        let config = NetworkBackendConfig {
            socket_path: PathBuf::from("/tmp/bl-box/net.sock"),
            allow_net: vec!["example.com".to_string()],
            secrets: Vec::new(),
            ca_dir: PathBuf::from("/tmp/bl-box/does-not-exist"),
            net_bandwidth: Default::default(),
        };
        let spec = GvproxyBackend::from_config(&config).spec();
        assert_eq!(spec.socket_path, config.socket_path);
        assert_eq!(spec.allow_net, config.allow_net);
        assert!(spec.ca_cert_pem.is_none());
        assert!(spec.ca_key_pem.is_none());
    }

    #[test]
    fn parse_stats_maps_gvproxy_stats_to_typed_getters() {
        // gvproxy /stats (PascalCase, bytes at root + TCP group) → typed getters.
        let json = r#"{"BytesSent":1024,"BytesReceived":2048,"TCP":{"ForwardMaxInFlightDrop":3,"CurrentEstablished":5,"FailedConnectionAttempts":2,"Retransmits":10,"Timeouts":1}}"#;
        let stats = parse_stats(json).unwrap();
        assert_eq!(stats.bytes_sent(), 1024);
        assert_eq!(stats.bytes_received(), 2048);
        assert_eq!(stats.tcp_established(), 5);
        assert_eq!(stats.tcp_failed_connections(), 2);
        assert_eq!(stats.tcp_retransmits(), 10);
        assert_eq!(stats.tcp_timeouts(), 1);
        assert_eq!(stats.tcp_forward_max_inflight_drop(), 3);
    }

    #[test]
    fn transport_protocol_wire_tokens() {
        assert_eq!(TransportProtocol::Tcp.as_str(), "tcp");
        assert_eq!(TransportProtocol::Udp.as_str(), "udp");
        assert_eq!(TransportProtocol::default(), TransportProtocol::Tcp);
        // serde agrees with as_str().
        assert_eq!(
            serde_json::to_string(&TransportProtocol::Udp).unwrap(),
            "\"udp\""
        );
    }

    #[test]
    fn parse_stats_rejects_malformed_body() {
        // The /stats parser fails loudly (echoing the body) on junk rather than
        // silently zeroing counters.
        let err = parse_stats("{ not json").unwrap_err();
        assert!(
            format!("{err}").contains("/stats parse failed"),
            "err: {err}"
        );
    }

    #[test]
    fn parse_stats_rejects_partial_stats() {
        // gvproxy's NetworkStats has no serde defaults, so a body missing the TCP
        // group is an error, not a partially-zeroed struct.
        assert!(parse_stats(r#"{"BytesSent":1,"BytesReceived":2}"#).is_err());
    }

    fn test_secret() -> crate::runtime::options::Secret {
        crate::runtime::options::Secret {
            name: "openai".to_string(),
            hosts: vec!["api.openai.com".to_string()],
            placeholder: "<BOXLITE_SECRET:openai>".to_string(),
            value: "sk-test-not-a-real-key".to_string(),
        }
    }

    #[derive(Debug)]
    struct CapturedRequest {
        request_line: String,
        body: String,
    }

    fn test_backend(
        dir: &tempfile::TempDir,
    ) -> (GvproxyBackend, std::path::PathBuf, NetworkBackendConfig) {
        let net_sock = dir.path().join("net.sock");
        let config = NetworkBackendConfig {
            socket_path: net_sock.clone(),
            allow_net: Vec::new(),
            secrets: Vec::new(),
            ca_dir: dir.path().to_path_buf(),
            net_bandwidth: Default::default(),
        };
        (
            GvproxyBackend::from_config(&config),
            super::super::control_socket_path(&net_sock),
            config,
        )
    }

    fn spawn_services_response(
        ctl: &std::path::Path,
        status: u16,
        body: impl Into<String>,
    ) -> tokio::task::JoinHandle<CapturedRequest> {
        use tokio::net::UnixListener;

        let body = body.into();
        let _ = std::fs::remove_file(ctl);
        let listener = UnixListener::bind(ctl).unwrap();
        tokio::spawn(async move {
            let (mut conn, _) = listener.accept().await.unwrap();
            let request = read_captured_request(&mut conn).await;
            write_services_response(&mut conn, status, &body).await;
            request
        })
    }

    fn spawn_services_responses(
        ctl: &std::path::Path,
        responses: Vec<(u16, String)>,
    ) -> tokio::task::JoinHandle<Vec<CapturedRequest>> {
        use tokio::net::UnixListener;

        let _ = std::fs::remove_file(ctl);
        let listener = UnixListener::bind(ctl).unwrap();
        tokio::spawn(async move {
            let mut requests = Vec::with_capacity(responses.len());
            for (status, body) in responses {
                let (mut conn, _) = listener.accept().await.unwrap();
                requests.push(read_captured_request(&mut conn).await);
                write_services_response(&mut conn, status, &body).await;
            }
            requests
        })
    }

    async fn read_captured_request(conn: &mut tokio::net::UnixStream) -> CapturedRequest {
        use tokio::io::AsyncReadExt;

        let mut headers = Vec::new();
        let mut byte = [0u8; 1];
        while !headers.ends_with(b"\r\n\r\n") {
            conn.read_exact(&mut byte).await.unwrap();
            headers.push(byte[0]);
        }

        let headers_text = String::from_utf8_lossy(&headers);
        let content_len = headers_text
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().unwrap())
            })
            .unwrap_or(0);
        let mut request_body = vec![0u8; content_len];
        if content_len > 0 {
            conn.read_exact(&mut request_body).await.unwrap();
        }

        CapturedRequest {
            request_line: headers_text.lines().next().unwrap().to_string(),
            body: String::from_utf8_lossy(&request_body).into_owned(),
        }
    }

    async fn write_services_response(conn: &mut tokio::net::UnixStream, status: u16, body: &str) {
        use tokio::io::AsyncWriteExt;

        let response = format!(
            "HTTP/1.1 {status} OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        conn.write_all(response.as_bytes()).await.unwrap();
    }

    fn json_body(req: &CapturedRequest) -> Value {
        serde_json::from_str(&req.body).unwrap()
    }

    #[test]
    fn spec_with_secrets_mints_a_ca_from_ca_dir() {
        // With secrets configured, spec() mints an ephemeral MITM CA into ca_dir
        // and threads the PEMs (and the secrets) onto the wire spec.
        let ca_dir = tempfile::tempdir().unwrap();
        let config = NetworkBackendConfig {
            socket_path: PathBuf::from("/tmp/bl-box/net.sock"),
            allow_net: Vec::new(),
            secrets: vec![test_secret()],
            ca_dir: ca_dir.path().to_path_buf(),
            net_bandwidth: Default::default(),
        };
        let spec = GvproxyBackend::from_config(&config).spec();
        assert!(
            spec.ca_cert_pem
                .as_deref()
                .unwrap()
                .contains("BEGIN CERTIFICATE"),
            "expected a minted CA cert"
        );
        assert!(
            spec.ca_key_pem.as_deref().unwrap().contains("PRIVATE KEY"),
            "expected a minted CA key"
        );
        assert_eq!(spec.secrets.len(), 1, "secrets flow onto the wire spec");
        // The CA was persisted under ca_dir (load_or_generate wrote it).
        assert!(ca_dir.path().join("cert.pem").exists());
    }

    #[test]
    fn spec_disables_secrets_when_ca_dir_is_unusable() {
        // The core must not hand secrets to gvproxy when it failed to provision
        // the MITM CA. Use a plain file as ca_dir so load_or_generate fails at
        // the project boundary, then assert spec() drops the secret material.
        let dir = tempfile::tempdir().unwrap();
        let ca_dir = dir.path().join("ca-dir-is-a-file");
        std::fs::write(&ca_dir, "not a directory").unwrap();
        let config = NetworkBackendConfig {
            socket_path: PathBuf::from("/tmp/bl-box/net.sock"),
            allow_net: Vec::new(),
            secrets: vec![test_secret()],
            ca_dir,
            net_bandwidth: Default::default(),
        };

        let spec = GvproxyBackend::from_config(&config).spec();

        assert!(
            spec.secrets.is_empty(),
            "secrets must be disabled when CA setup fails"
        );
        assert!(spec.ca_cert_pem.is_none());
        assert!(spec.ca_key_pem.is_none());
    }

    #[tokio::test]
    async fn list_forwards_reports_missing_services_socket_path() {
        let dir = tempfile::Builder::new()
            .prefix("bl-svctest-missing-")
            .tempdir_in("/tmp")
            .unwrap();
        let (backend, ctl, _) = test_backend(&dir);

        let err = backend.list_forwards().await.unwrap_err();

        let err = format!("{err}");
        assert!(err.contains("gvproxy services connect"), "err: {err}");
        assert!(err.contains(&ctl.display().to_string()), "err: {err}");
    }

    #[tokio::test]
    async fn tunnel_reports_missing_services_socket_path() {
        let dir = tempfile::Builder::new()
            .prefix("bl-tuntest-missing-")
            .tempdir_in("/tmp")
            .unwrap();
        let (backend, ctl, _) = test_backend(&dir);
        let target: SocketAddr = "192.168.127.2:8080".parse().unwrap();

        let err = backend.tunnel(target).await.unwrap_err();

        let err = format!("{err}");
        assert!(err.contains("gvproxy tunnel connect"), "err: {err}");
        assert!(err.contains(&ctl.display().to_string()), "err: {err}");
    }

    #[tokio::test]
    async fn expose_fixed_posts_once_and_returns_concrete_forward() {
        let dir = tempfile::Builder::new()
            .prefix("bl-svctest-")
            .tempdir_in("/tmp")
            .unwrap();
        let (backend, ctl, _) = test_backend(&dir);
        let server = spawn_services_response(&ctl, 204, "");

        let forward = backend
            .expose(
                "127.0.0.1:18080",
                "192.168.127.2:80",
                TransportProtocol::Udp,
            )
            .await
            .unwrap();

        let req = server.await.unwrap();
        assert_eq!(req.request_line, "POST /services/forwarder/expose HTTP/1.1");
        let body = json_body(&req);
        assert_eq!(body["local"], "127.0.0.1:18080");
        assert_eq!(body["remote"], "192.168.127.2:80");
        assert_eq!(body["protocol"], "udp");
        assert_eq!(
            forward,
            Forward {
                local: "127.0.0.1:18080".to_string(),
                remote: "192.168.127.2:80".to_string(),
                protocol: "udp".to_string(),
            }
        );
    }

    #[tokio::test]
    async fn expose_auto_posts_concrete_local_endpoint() {
        let dir = tempfile::Builder::new()
            .prefix("bl-svctest-")
            .tempdir_in("/tmp")
            .unwrap();
        let (backend, ctl, _) = test_backend(&dir);
        let server = spawn_services_response(&ctl, 204, "");

        backend
            .expose("127.0.0.1:0", "192.168.127.2:80", TransportProtocol::Tcp)
            .await
            .unwrap();

        let request = server.await.unwrap();
        let posted_local = json_body(&request)["local"]
            .as_str()
            .unwrap()
            .parse::<SocketAddr>()
            .unwrap();
        assert_ne!(
            posted_local.port(),
            0,
            "gvproxy must receive the concrete host port, not the unresolved :0 request"
        );
    }

    #[tokio::test]
    async fn expose_auto_returns_nonzero_port_after_releasing_reservation() {
        use tokio::net::UnixListener;

        let dir = tempfile::Builder::new()
            .prefix("bl-svctest-")
            .tempdir_in("/tmp")
            .unwrap();
        let (backend, ctl, _) = test_backend(&dir);
        let listener = UnixListener::bind(&ctl).unwrap();
        let server = tokio::spawn(async move {
            let (mut conn, _) = listener.accept().await.unwrap();
            let request = read_captured_request(&mut conn).await;
            let local: SocketAddr = json_body(&request)["local"]
                .as_str()
                .unwrap()
                .parse()
                .unwrap();
            let reservation_was_released = TcpListener::bind(local).is_ok();
            write_services_response(&mut conn, 204, "").await;
            (request, reservation_was_released)
        });

        let forward = backend
            .expose("127.0.0.1:0", "192.168.127.2:80", TransportProtocol::Tcp)
            .await
            .unwrap();

        let (request, reservation_was_released) = server.await.unwrap();
        let posted_local: SocketAddr = json_body(&request)["local"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        assert_ne!(posted_local.port(), 0);
        assert_eq!(forward.local, posted_local.to_string());
        assert_eq!(forward.remote, "192.168.127.2:80");
        assert_eq!(forward.protocol, "tcp");
        assert!(
            reservation_was_released,
            "gvproxy must be able to bind the selected port while handling the request"
        );
    }

    #[tokio::test]
    async fn expose_protocol_contract_rejects_tcp_local_without_ip() {
        let dir = tempfile::Builder::new()
            .prefix("bl-svctest-")
            .tempdir_in("/tmp")
            .unwrap();
        let (backend, _, _) = test_backend(&dir);

        let error = backend
            .expose(":0", "192.168.127.2:80", TransportProtocol::Tcp)
            .await
            .unwrap_err();

        let BoxliteError::Config(message) = error else {
            panic!("expected Config, got {error:?}");
        };
        assert!(message.contains("invalid TCP local endpoint"));
        assert!(message.contains("expected IP:port"));
    }

    /// A released TCP reservation would leave a UDP port free too, so the
    /// end-to-end UDP test cannot prove the reservation used the right
    /// transport. Check the socket family directly.
    #[test]
    fn expose_protocol_contract_reserves_with_matching_transport() {
        let requested: SocketAddr = "127.0.0.1:0".parse().unwrap();

        let tcp = HostPortReservation::bind(requested, TransportProtocol::Tcp).unwrap();
        assert!(matches!(tcp, HostPortReservation::Tcp(_)));

        let udp = HostPortReservation::bind(requested, TransportProtocol::Udp).unwrap();
        assert!(matches!(udp, HostPortReservation::Udp(_)));
    }

    #[tokio::test]
    async fn expose_protocol_contract_udp_auto_returns_nonzero_udp_forward() {
        use tokio::net::UnixListener;

        let dir = tempfile::Builder::new()
            .prefix("bl-svctest-")
            .tempdir_in("/tmp")
            .unwrap();
        let (backend, ctl, _) = test_backend(&dir);
        let listener = UnixListener::bind(&ctl).unwrap();
        let server = tokio::spawn(async move {
            let (mut conn, _) = listener.accept().await.unwrap();
            let request = read_captured_request(&mut conn).await;
            let local: SocketAddr = json_body(&request)["local"]
                .as_str()
                .unwrap()
                .parse()
                .unwrap();
            let reservation_was_released = std::net::UdpSocket::bind(local).is_ok();
            write_services_response(&mut conn, 204, "").await;
            (request, reservation_was_released)
        });

        let forward = backend
            .expose("127.0.0.1:0", "192.168.127.2:53", TransportProtocol::Udp)
            .await
            .unwrap();

        let (request, reservation_was_released) = server.await.unwrap();
        let body = json_body(&request);
        let posted_local: SocketAddr = body["local"].as_str().unwrap().parse().unwrap();
        assert_ne!(posted_local.port(), 0);
        assert_eq!(body["protocol"], "udp");
        assert_eq!(forward.local, posted_local.to_string());
        assert_eq!(forward.protocol, "udp");
        assert!(
            reservation_was_released,
            "gvproxy must be able to bind the selected UDP port"
        );
    }

    #[tokio::test]
    async fn expose_protocol_contract_keeps_unix_local_opaque() {
        let dir = tempfile::Builder::new()
            .prefix("bl-svctest-")
            .tempdir_in("/tmp")
            .unwrap();
        let (backend, ctl, _) = test_backend(&dir);
        let server = spawn_services_response(&ctl, 204, "");

        let forward = backend
            .expose(
                "127.0.0.1:0",
                "tcp://192.168.127.2:80",
                TransportProtocol::Unix,
            )
            .await
            .unwrap();

        let request = server.await.unwrap();
        let body = json_body(&request);
        assert_eq!(body["local"], "127.0.0.1:0");
        assert_eq!(body["protocol"], "unix");
        assert_eq!(forward.local, "127.0.0.1:0");
        assert_eq!(forward.protocol, "unix");
    }

    #[tokio::test]
    async fn expose_fixed_reports_bind_conflict_as_already_exists_without_retry() {
        let dir = tempfile::Builder::new()
            .prefix("bl-svctest-")
            .tempdir_in("/tmp")
            .unwrap();
        let (backend, ctl, _) = test_backend(&dir);
        let server = spawn_services_response(&ctl, 500, "proxy already running");

        let error = backend
            .expose(
                "127.0.0.1:18080",
                "192.168.127.2:80",
                TransportProtocol::Tcp,
            )
            .await
            .unwrap_err();

        assert_eq!(
            server.await.unwrap().request_line,
            "POST /services/forwarder/expose HTTP/1.1"
        );
        let BoxliteError::AlreadyExists(message) = error else {
            panic!("expected AlreadyExists, got {error:?}");
        };
        assert!(message.contains("returned 500"));
        assert!(message.contains("proxy already running"));
    }

    #[tokio::test]
    async fn expose_auto_retries_known_proxy_already_running_response() {
        let dir = tempfile::Builder::new()
            .prefix("bl-svctest-")
            .tempdir_in("/tmp")
            .unwrap();
        let (backend, ctl, _) = test_backend(&dir);
        let server = spawn_services_responses(
            &ctl,
            vec![
                (500, "proxy already running".to_string()),
                (204, String::new()),
            ],
        );

        let forward = backend
            .expose("127.0.0.1:0", "192.168.127.2:80", TransportProtocol::Tcp)
            .await
            .unwrap();

        let requests = server.await.unwrap();
        assert_eq!(requests.len(), 2);
        assert!(requests.iter().all(|request| {
            json_body(request)["local"]
                .as_str()
                .unwrap()
                .parse::<SocketAddr>()
                .unwrap()
                .port()
                != 0
        }));
        assert_eq!(
            forward.local,
            json_body(&requests[1])["local"].as_str().unwrap()
        );
    }

    #[tokio::test]
    async fn expose_auto_stops_after_five_bind_conflicts() {
        let dir = tempfile::Builder::new()
            .prefix("bl-svctest-")
            .tempdir_in("/tmp")
            .unwrap();
        let (backend, ctl, _) = test_backend(&dir);
        let server = spawn_services_responses(
            &ctl,
            (0..EXPOSE_AUTO_PORT_ATTEMPTS)
                .map(|_| {
                    (
                        500,
                        "listen tcp 127.0.0.1:1234: bind: address already in use".to_string(),
                    )
                })
                .collect(),
        );

        let error = backend
            .expose("127.0.0.1:0", "192.168.127.2:80", TransportProtocol::Tcp)
            .await
            .unwrap_err();

        let requests = server.await.unwrap();
        assert_eq!(requests.len(), EXPOSE_AUTO_PORT_ATTEMPTS);
        let BoxliteError::AlreadyExists(message) = error else {
            panic!("expected AlreadyExists, got {error:?}");
        };
        assert!(message.contains("exhausted 5 automatic port attempts"));
        assert!(message.contains("bind: address already in use"));
    }

    #[tokio::test]
    async fn expose_auto_does_not_retry_unrelated_500_response() {
        let dir = tempfile::Builder::new()
            .prefix("bl-svctest-")
            .tempdir_in("/tmp")
            .unwrap();
        let (backend, ctl, _) = test_backend(&dir);
        let server = spawn_services_response(&ctl, 500, "internal forwarder failure");

        let error = backend
            .expose("127.0.0.1:0", "192.168.127.2:80", TransportProtocol::Tcp)
            .await
            .unwrap_err();

        assert_eq!(
            server.await.unwrap().request_line,
            "POST /services/forwarder/expose HTTP/1.1"
        );
        let BoxliteError::Network(message) = error else {
            panic!("expected Network, got {error:?}");
        };
        assert!(message.contains("returned 500"));
        assert!(message.contains("internal forwarder failure"));
        assert!(!message.contains("exhausted"));
    }

    #[tokio::test]
    async fn unexpose_posts_local_and_protocol_to_services_socket() {
        let dir = tempfile::Builder::new()
            .prefix("bl-svctest-")
            .tempdir_in("/tmp")
            .unwrap();
        let (backend, ctl, _) = test_backend(&dir);
        let server = spawn_services_response(&ctl, 200, "");

        backend
            .unexpose("127.0.0.1:18080", TransportProtocol::Tcp)
            .await
            .unwrap();

        let req = server.await.unwrap();
        assert_eq!(
            req.request_line,
            "POST /services/forwarder/unexpose HTTP/1.1"
        );
        let body = json_body(&req);
        assert_eq!(body["local"], "127.0.0.1:18080");
        assert_eq!(body["protocol"], "tcp");
        assert!(body.get("remote").is_none());
    }

    #[tokio::test]
    async fn add_dns_zone_posts_gvproxy_zone_wire_shape() {
        let dir = tempfile::Builder::new()
            .prefix("bl-svctest-")
            .tempdir_in("/tmp")
            .unwrap();
        let (backend, ctl, _) = test_backend(&dir);
        let server = spawn_services_response(&ctl, 200, "");

        backend
            .add_dns_zone(DnsZoneSpec {
                name: "svc.local.".to_string(),
                records: vec![DnsRecordSpec {
                    name: "api".to_string(),
                    ip: "192.168.127.50".to_string(),
                }],
                default_ip: Some("192.168.127.254".to_string()),
            })
            .await
            .unwrap();

        let req = server.await.unwrap();
        assert_eq!(req.request_line, "POST /services/dns/add HTTP/1.1");
        let body = json_body(&req);
        assert_eq!(body["Name"], "svc.local.");
        assert_eq!(body["DefaultIP"], "192.168.127.254");
        assert_eq!(body["Records"][0]["Name"], "api");
        assert_eq!(body["Records"][0]["IP"], "192.168.127.50");
        assert!(body.get("default_ip").is_none());
    }

    #[tokio::test]
    async fn list_forwards_gets_and_parses_forwarder_all() {
        let dir = tempfile::Builder::new()
            .prefix("bl-svctest-")
            .tempdir_in("/tmp")
            .unwrap();
        let (backend, ctl, _) = test_backend(&dir);
        let body = r#"[{"local":"127.0.0.1:2222","remote":"192.168.127.2:22","protocol":"tcp"}]"#;
        let server = spawn_services_response(&ctl, 200, body);

        let forwards = backend.list_forwards().await.unwrap();

        let req = server.await.unwrap();
        assert_eq!(req.request_line, "GET /services/forwarder/all HTTP/1.1");
        assert_eq!(
            forwards,
            vec![Forward {
                local: "127.0.0.1:2222".to_string(),
                remote: "192.168.127.2:22".to_string(),
                protocol: "tcp".to_string(),
            }]
        );
    }

    #[tokio::test]
    async fn json_read_endpoints_get_expected_paths_and_parse_bodies() {
        let dir = tempfile::Builder::new()
            .prefix("bl-svctest-")
            .tempdir_in("/tmp")
            .unwrap();
        let (backend, ctl, _) = test_backend(&dir);

        let server = spawn_services_response(&ctl, 200, r#"{"Zones":["svc.local."]}"#);
        assert_eq!(backend.dns_zones().await.unwrap()["Zones"][0], "svc.local.");
        assert_eq!(
            server.await.unwrap().request_line,
            "GET /services/dns/all HTTP/1.1"
        );

        let server = spawn_services_response(&ctl, 200, r#"{"Leases":[{"IP":"192.168.127.2"}]}"#);
        assert_eq!(
            backend.dhcp_leases().await.unwrap()["Leases"][0]["IP"],
            "192.168.127.2"
        );
        assert_eq!(
            server.await.unwrap().request_line,
            "GET /services/dhcp/leases HTTP/1.1"
        );

        let server = spawn_services_response(&ctl, 200, r#"{"Table":{"aa:bb":"tap0"}}"#);
        assert_eq!(backend.cam().await.unwrap()["Table"]["aa:bb"], "tap0");
        assert_eq!(server.await.unwrap().request_line, "GET /cam HTTP/1.1");
    }

    #[tokio::test]
    async fn stats_gets_and_maps_services_socket_response() {
        let dir = tempfile::Builder::new()
            .prefix("bl-svctest-")
            .tempdir_in("/tmp")
            .unwrap();
        let (backend, ctl, _) = test_backend(&dir);
        let body = r#"{"BytesSent":7,"BytesReceived":11,"TCP":{"ForwardMaxInFlightDrop":13,"CurrentEstablished":17,"FailedConnectionAttempts":19,"Retransmits":23,"Timeouts":29}}"#;
        let server = spawn_services_response(&ctl, 200, body);

        let stats = backend.stats().await.unwrap();

        assert_eq!(server.await.unwrap().request_line, "GET /stats HTTP/1.1");
        assert_eq!(stats.bytes_sent(), 7);
        assert_eq!(stats.bytes_received(), 11);
        assert_eq!(stats.tcp_established(), 17);
        assert_eq!(stats.tcp_failed_connections(), 19);
        assert_eq!(stats.tcp_retransmits(), 23);
        assert_eq!(stats.tcp_timeouts(), 29);
        assert_eq!(stats.tcp_forward_max_inflight_drop(), 13);
    }

    #[tokio::test]
    async fn non_success_control_response_includes_path_status_and_body() {
        let dir = tempfile::Builder::new()
            .prefix("bl-svctest-")
            .tempdir_in("/tmp")
            .unwrap();
        let (backend, ctl, _) = test_backend(&dir);
        let server = spawn_services_response(&ctl, 409, "proxy already running");

        let err = backend
            .expose(
                "127.0.0.1:18080",
                "192.168.127.2:80",
                TransportProtocol::Tcp,
            )
            .await
            .unwrap_err();

        assert_eq!(
            server.await.unwrap().request_line,
            "POST /services/forwarder/expose HTTP/1.1"
        );
        let err = format!("{err}");
        assert!(err.contains("/services/forwarder/expose"), "err: {err}");
        assert!(err.contains("409"), "err: {err}");
        assert!(err.contains("proxy already running"), "err: {err}");
    }

    #[tokio::test]
    async fn list_forwards_rejects_malformed_json_response() {
        let dir = tempfile::Builder::new()
            .prefix("bl-svctest-")
            .tempdir_in("/tmp")
            .unwrap();
        let (backend, ctl, _) = test_backend(&dir);
        let server = spawn_services_response(&ctl, 200, "not-json");

        let err = backend.list_forwards().await.unwrap_err();

        assert_eq!(
            server.await.unwrap().request_line,
            "GET /services/forwarder/all HTTP/1.1"
        );
        let err = format!("{err}");
        assert!(err.contains("/services/forwarder/all parse failed"));
        assert!(err.contains("not-json"));
    }

    #[tokio::test]
    async fn json_read_endpoints_reject_malformed_json_response() {
        let dir = tempfile::Builder::new()
            .prefix("bl-svctest-")
            .tempdir_in("/tmp")
            .unwrap();
        let (backend, ctl, _) = test_backend(&dir);
        let server = spawn_services_response(&ctl, 200, "not-json");

        let err = backend.cam().await.unwrap_err();

        assert_eq!(server.await.unwrap().request_line, "GET /cam HTTP/1.1");
        let err = format!("{err}");
        assert!(err.contains("/cam parse failed"));
        assert!(err.contains("not-json"));
    }

    /// The raw-hijack `/tunnel` client, exercised without a live gvproxy: a fake
    /// control server speaks the `POST /tunnel` + `"OK"` handshake, then echoes —
    /// proving the request wire format, the ack, and that the returned tunnel is a
    /// live bidirectional pipe carrying the target as its peer.
    #[tokio::test]
    async fn tunnel_speaks_raw_hijack_and_returns_a_live_pipe() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::UnixListener;

        // Short socket dir (sun_path budget), auto-removed on drop.
        let dir = tempfile::Builder::new()
            .prefix("bl-tuntest-")
            .tempdir_in("/tmp")
            .unwrap();
        let net_sock = dir.path().join("net.sock");
        // Bind the fake server at the exact control socket the backend will dial.
        let ctl = super::super::control_socket_path(&net_sock);
        let listener = UnixListener::bind(&ctl).unwrap();

        let server = tokio::spawn(async move {
            let (mut conn, _) = listener.accept().await.unwrap();
            // Read the request line + headers up to the blank line.
            let mut req = Vec::new();
            let mut b = [0u8; 1];
            while !req.ends_with(b"\r\n\r\n") {
                conn.read_exact(&mut b).await.unwrap();
                req.push(b[0]);
            }
            conn.write_all(b"OK").await.unwrap();
            // Prove the socket is a live pipe after the handshake.
            let mut msg = [0u8; 4];
            conn.read_exact(&mut msg).await.unwrap();
            conn.write_all(&msg).await.unwrap();
            String::from_utf8_lossy(&req).into_owned()
        });

        let config = NetworkBackendConfig {
            socket_path: net_sock,
            allow_net: Vec::new(),
            secrets: Vec::new(),
            ca_dir: dir.path().to_path_buf(),
            net_bandwidth: Default::default(),
        };
        let target: SocketAddr = "192.168.127.2:8080".parse().unwrap();
        let mut tunnel = GvproxyBackend::from_config(&config)
            .tunnel(target)
            .await
            .expect("handshake ok");

        assert_eq!(tunnel.peer_addr(), target);
        tunnel.write_all(b"ping").await.unwrap();
        let mut echoed = [0u8; 4];
        tunnel.read_exact(&mut echoed).await.unwrap();
        assert_eq!(&echoed, b"ping");

        let req = server.await.unwrap();
        assert!(
            req.starts_with("POST /tunnel?ip=192.168.127.2&port=8080 HTTP/1.1"),
            "unexpected request line: {req}"
        );
    }

    /// A non-`"OK"` ack must surface as an error, not a half-open tunnel.
    #[tokio::test]
    async fn tunnel_errors_when_ack_is_not_ok() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::UnixListener;

        let dir = tempfile::Builder::new()
            .prefix("bl-tuntest-")
            .tempdir_in("/tmp")
            .unwrap();
        let net_sock = dir.path().join("net.sock");
        let ctl = super::super::control_socket_path(&net_sock);
        let listener = UnixListener::bind(&ctl).unwrap();
        tokio::spawn(async move {
            let (mut conn, _) = listener.accept().await.unwrap();
            let mut b = [0u8; 1];
            let mut req = Vec::new();
            while !req.ends_with(b"\r\n\r\n") {
                conn.read_exact(&mut b).await.unwrap();
                req.push(b[0]);
            }
            conn.write_all(b"NO").await.unwrap(); // reject
        });

        let config = NetworkBackendConfig {
            socket_path: net_sock,
            allow_net: Vec::new(),
            secrets: Vec::new(),
            ca_dir: dir.path().to_path_buf(),
            net_bandwidth: Default::default(),
        };
        let target: SocketAddr = "192.168.127.2:8080".parse().unwrap();
        let err = GvproxyBackend::from_config(&config)
            .tunnel(target)
            .await
            .unwrap_err();
        assert!(format!("{err}").contains(r#"expected "OK""#), "err: {err}");
    }

    /// End-to-end over a live gvproxy instance: the core dials the services
    /// socket to expose a forward, sees it in `/all`, unexposes it, and sees it
    /// gone. No VM is needed — the ServicesMux answers independently of the tap.
    /// Requires the libgvproxy dylib; run with `--ignored`.
    #[cfg(feature = "gvproxy")]
    #[tokio::test]
    #[ignore]
    async fn expose_unexpose_roundtrip_over_services_socket() {
        use crate::net::gvproxy::GvproxyInstance;

        // Short socket dir (sun_path budget); auto-removed on drop.
        let dir = tempfile::Builder::new()
            .prefix("bl-svc-test-")
            .tempdir_in("/tmp")
            .unwrap();
        let net_sock = dir.path().join("net.sock");

        // Bind a real gvproxy instance; it derives + serves its control socket
        // (`gvproxy-ctl.sock`) as a sibling of net.sock.
        let _instance = GvproxyInstance::new(
            crate::net::gvproxy::config::GvproxyConfig::new(net_sock.clone())
                .with_control_socket_path(super::super::control_socket_path(&net_sock)),
        )
        .expect("create gvproxy instance");

        let config = NetworkBackendConfig {
            socket_path: net_sock.clone(),
            allow_net: Vec::new(),
            secrets: Vec::new(),
            ca_dir: dir.path().to_path_buf(),
            net_bandwidth: Default::default(),
        };
        let ctl = GvproxyBackend::from_config(&config);

        // `gvproxy_create` returns only once the services socket is listening,
        // so a single call must succeed.
        ctl.list_forwards()
            .await
            .expect("services socket is bound before create returns");

        let local = "127.0.0.1:18080";
        let has = |fs: &[Forward]| fs.iter().any(|f| f.local == local);

        assert!(
            !has(&ctl.list_forwards().await.unwrap()),
            "forward should be absent before expose"
        );

        ctl.expose(local, "192.168.127.2:80", TransportProtocol::Tcp)
            .await
            .expect("expose");
        assert!(
            has(&ctl.list_forwards().await.unwrap()),
            "forward should be present after expose"
        );

        ctl.unexpose(local, TransportProtocol::Tcp)
            .await
            .expect("unexpose");
        assert!(
            !has(&ctl.list_forwards().await.unwrap()),
            "forward should be gone after unexpose"
        );
    }

    /// End-to-end over a live gvproxy instance: dial `/tunnel` and verify the
    /// `"OK"` handshake. gvproxy writes `"OK"` *before* it dials the guest, so the
    /// handshake completes even with no guest listening — enough to exercise the
    /// raw-hijack protocol without a VM. Requires the libgvproxy dylib; `--ignored`.
    #[cfg(feature = "gvproxy")]
    #[tokio::test]
    #[ignore]
    async fn tunnel_handshake_over_services_socket() {
        use crate::net::gvproxy::GvproxyInstance;

        let dir = tempfile::Builder::new()
            .prefix("bl-tun-test-")
            .tempdir_in("/tmp")
            .unwrap();
        let net_sock = dir.path().join("net.sock");
        let _instance = GvproxyInstance::new(
            crate::net::gvproxy::config::GvproxyConfig::new(net_sock.clone())
                .with_control_socket_path(super::super::control_socket_path(&net_sock)),
        )
        .expect("create gvproxy instance");

        let config = NetworkBackendConfig {
            socket_path: net_sock.clone(),
            allow_net: Vec::new(),
            secrets: Vec::new(),
            ca_dir: dir.path().to_path_buf(),
            net_bandwidth: Default::default(),
        };
        let backend = GvproxyBackend::from_config(&config);

        // `gvproxy_create` returns only once the services socket is listening,
        // so a single call must succeed.
        backend
            .list_forwards()
            .await
            .expect("services socket is bound before create returns");

        let target: SocketAddr = "192.168.127.2:8080".parse().unwrap();
        let tunnel = backend.tunnel(target).await.expect("tunnel handshake");
        assert_eq!(tunnel.peer_addr(), target);
        // The owned OS fd is recoverable for an SDK handoff.
    }
}
