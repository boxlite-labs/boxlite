#![cfg(feature = "gvproxy")]

//! Live gvproxy backend integration tests.
//!
//! These exercise the cross-process contract between the core-side
//! `GvproxyBackend` control client and a real shim-side `GvproxyInstance`.

use std::io::ErrorKind;
use std::net::{SocketAddr, TcpListener};
use std::path::PathBuf;
use std::sync::mpsc::RecvTimeoutError;
use std::time::Duration;

use boxlite::net::gvproxy::GvproxyInstance;
use boxlite::net::{
    GvproxyBackend, NetworkBackend, NetworkBackendConfig, NetworkBackendSpec, TransportProtocol,
};
use notify::Watcher;

const SERVICES_SOCKET_READY_TIMEOUT: Duration = Duration::from_secs(30);

fn backend_for(
    dir: &tempfile::TempDir,
) -> (
    GvproxyInstance,
    GvproxyBackend,
    boxlite::net::NetworkBackendEndpoint,
    PathBuf,
) {
    let net_sock = dir.path().join("net.sock");
    let control_sock = dir.path().join("gvproxy-ctl.sock");
    let spec = NetworkBackendSpec {
        socket_path: net_sock.clone(),
        allow_net: Vec::new(),
        secrets: Vec::new(),
        ca_cert_pem: None,
        ca_key_pem: None,
        net_bandwidth: Default::default(),
    };
    let (instance, endpoint) = GvproxyInstance::from_config(&spec).expect("create gvproxy");
    let config = NetworkBackendConfig {
        socket_path: net_sock,
        allow_net: Vec::new(),
        secrets: Vec::new(),
        ca_dir: dir.path().to_path_buf(),
        net_bandwidth: Default::default(),
    };
    (
        instance,
        GvproxyBackend::from_config(&config),
        endpoint,
        control_sock,
    )
}

async fn wait_for_services(backend: &GvproxyBackend, control_sock: PathBuf) {
    if backend.list_forwards().await.is_ok() {
        return;
    }

    let socket_for_watch = control_sock.clone();
    tokio::task::spawn_blocking(move || wait_for_socket_file(socket_for_watch))
        .await
        .expect("wait for gvproxy services socket task")
        .unwrap_or_else(|err| panic!("{err}"));

    backend.list_forwards().await.unwrap_or_else(|err| {
        panic!(
            "gvproxy services socket {} was created but not reachable: {err}",
            control_sock.display()
        )
    });
}

fn wait_for_socket_file(control_sock: PathBuf) -> Result<(), String> {
    if control_sock.exists() {
        return Ok(());
    }

    let parent = control_sock
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", control_sock.display()))?
        .to_path_buf();
    let socket_name = control_sock
        .file_name()
        .ok_or_else(|| format!("{} has no file name", control_sock.display()))?
        .to_os_string();
    let (tx, rx) = std::sync::mpsc::channel();
    let mut watcher = notify::RecommendedWatcher::new(
        move |event| {
            let _ = tx.send(event);
        },
        notify::Config::default(),
    )
    .map_err(|err| format!("watch {} failed: {err}", parent.display()))?;
    watcher
        .watch(&parent, notify::RecursiveMode::NonRecursive)
        .map_err(|err| format!("watch {} failed: {err}", parent.display()))?;

    let deadline = std::time::Instant::now() + SERVICES_SOCKET_READY_TIMEOUT;
    loop {
        if control_sock.exists() {
            return Ok(());
        }

        let remaining = deadline
            .checked_duration_since(std::time::Instant::now())
            .ok_or_else(|| {
                format!(
                    "gvproxy services socket {} never became reachable",
                    control_sock.display()
                )
            })?;

        match rx.recv_timeout(remaining) {
            Ok(Ok(event)) => {
                let saw_socket = event.paths.iter().any(|path| {
                    path == &control_sock || path.file_name() == Some(socket_name.as_os_str())
                });
                if saw_socket && control_sock.exists() {
                    return Ok(());
                }
            }
            Ok(Err(err)) => {
                return Err(format!("watch {} failed: {err}", parent.display()));
            }
            Err(RecvTimeoutError::Timeout) => {
                return Err(format!(
                    "gvproxy services socket {} never became reachable",
                    control_sock.display()
                ));
            }
            Err(RecvTimeoutError::Disconnected) => {
                return Err(format!("watch {} stopped unexpectedly", parent.display()));
            }
        }
    }
}

#[tokio::test]
async fn live_gvproxy_backend_expose_list_unexpose_roundtrip() {
    let dir = tempfile::Builder::new()
        .prefix("bl-live-gvproxy-")
        .tempdir_in("/tmp")
        .unwrap();
    let (_instance, backend, endpoint, control_sock) = backend_for(&dir);
    wait_for_services(&backend, control_sock).await;

    match endpoint {
        boxlite::net::NetworkBackendEndpoint::UnixSocket { path, .. } => {
            assert_eq!(path, dir.path().join("net.sock"));
        }
    }

    let local_socket = dir.path().join("forward.sock");
    let local = local_socket.display().to_string();
    let has_local =
        |forwards: &[boxlite::net::Forward]| forwards.iter().any(|forward| forward.local == local);

    assert!(
        !has_local(&backend.list_forwards().await.unwrap()),
        "forward should be absent before expose"
    );

    backend
        .expose(&local, "tcp://192.168.127.2:80", TransportProtocol::Unix)
        .await
        .expect("expose forward");
    assert!(
        has_local(&backend.list_forwards().await.unwrap()),
        "forward should be present after expose"
    );

    backend
        .unexpose(&local, TransportProtocol::Unix)
        .await
        .expect("unexpose forward");
    assert!(
        !has_local(&backend.list_forwards().await.unwrap()),
        "forward should be absent after unexpose"
    );
}

/// A host port the OS refuses on permission grounds must surface gvproxy's own
/// bind failure through `/services/forwarder/expose`, not a synthesized
/// conflict string. Companion to the CLI suite's busy-port test, which covers
/// the EADDRINUSE half of the same contract.
///
/// This has to live here rather than in the CLI suite: `-p` cannot request a
/// specific host address, leaving `PortSpec::host_ip` unset, which resolves to
/// the wildcard — and Darwin applies the reserved-port check only to a named
/// address, so a wildcard bind of port 80 succeeds there. Naming an address is
/// refused on Darwin, and on any host that restricts privileged ports whatever
/// the address, so this is where the permission path stays reachable; the
/// guard below skips wherever the bind is permitted.
#[tokio::test]
async fn live_gvproxy_backend_expose_privileged_port_surfaces_permission_denied() {
    // The premise is "binding a privileged port is refused *on permission
    // grounds*". Root, a CAP_NET_BIND_SERVICE binary, or a lowered
    // `ip_unprivileged_port_start` removes it. So does an unrelated failure:
    // if something already holds :80 the bind fails with AddrInUse, and
    // treating that as the premise would assert a permission error gvproxy is
    // never going to produce. Proceed only on an explicit PermissionDenied.
    match TcpListener::bind("127.0.0.1:80") {
        Err(error) if error.kind() == ErrorKind::PermissionDenied => {}
        Ok(_) => {
            eprintln!(
                "SKIP live_gvproxy_backend_expose_privileged_port_surfaces_permission_denied: \
                 host allows binding 127.0.0.1:80 (root / CAP_NET_BIND_SERVICE / \
                 low ip_unprivileged_port_start)"
            );
            return;
        }
        Err(error) => {
            eprintln!(
                "SKIP live_gvproxy_backend_expose_privileged_port_surfaces_permission_denied: \
                 probing 127.0.0.1:80 failed with {:?} ({error}), not PermissionDenied, \
                 so the premise cannot be established",
                error.kind()
            );
            return;
        }
    }

    let dir = tempfile::Builder::new()
        .prefix("bl-live-gvproxy-privport-")
        .tempdir_in("/tmp")
        .unwrap();
    let (_instance, backend, _, control_sock) = backend_for(&dir);
    wait_for_services(&backend, control_sock).await;

    let local = "127.0.0.1:80";
    let error = backend
        .expose(local, "192.168.127.2:80", TransportProtocol::Tcp)
        .await
        .expect_err("a privileged host port must not publish");

    let error = format!("{error}");
    assert!(
        error.contains("/services/forwarder/expose"),
        "error should name the endpoint that failed: {error}"
    );
    assert!(
        error.to_ascii_lowercase().contains("permission denied"),
        "error should carry the OS bind failure verbatim, not a generic \
         conflict string: {error}"
    );
    assert!(
        !backend
            .list_forwards()
            .await
            .expect("list forwards")
            .iter()
            .any(|forward| forward.local == local),
        "a refused expose must not leave a forward registered"
    );
}

#[tokio::test]
async fn live_gvproxy_backend_tunnel_handshake_returns_fd() {
    let dir = tempfile::Builder::new()
        .prefix("bl-live-gvproxy-tunnel-")
        .tempdir_in("/tmp")
        .unwrap();
    let (_instance, backend, _, control_sock) = backend_for(&dir);
    wait_for_services(&backend, control_sock).await;

    let target: SocketAddr = "192.168.127.2:8080".parse().unwrap();
    let tunnel = backend.tunnel(target).await.expect("tunnel handshake");

    assert_eq!(tunnel.peer_addr(), target);
}
