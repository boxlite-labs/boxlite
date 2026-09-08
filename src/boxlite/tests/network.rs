//! Integration tests for the network backend abstraction.

use std::path::PathBuf;

use boxlite::net::{NetworkBackendConfig, NetworkBackendSpec};

fn test_config(socket_path: PathBuf) -> NetworkBackendConfig {
    NetworkBackendConfig {
        socket_path,
        allow_net: Vec::new(),
        secrets: Vec::new(),
        ca_dir: PathBuf::from("/tmp/test-ca"),
        net_bandwidth: Default::default(),
    }
}

#[test]
fn spec_carries_unique_socket_path_across_serde() {
    // The wire spec carries a unique per-box socket path across the serde
    // boundary to the shim (guards the old gvproxy socket collision, where the
    // Go library generated /tmp/gvproxy-{id}.sock).
    let spec = NetworkBackendSpec {
        socket_path: PathBuf::from("/boxes/box-a/sockets/net.sock"),
        allow_net: Vec::new(),
        secrets: Vec::new(),
        ca_cert_pem: None,
        ca_key_pem: None,
        net_bandwidth: Default::default(),
    };

    // socket_path survives serde — this is how it crosses to the shim.
    let json = serde_json::to_string(&spec).unwrap();
    let deserialized: NetworkBackendSpec = serde_json::from_str(&json).unwrap();
    assert_eq!(deserialized.socket_path, spec.socket_path);
}

#[test]
fn spec_deserializes_legacy_payload_with_default_control_fields() {
    let json = r#"{"port_mappings":[[8080,80]],"socket_path":"/boxes/box-a/sockets/net.sock"}"#;
    let spec: NetworkBackendSpec = serde_json::from_str(json).unwrap();

    assert_eq!(
        spec.socket_path,
        PathBuf::from("/boxes/box-a/sockets/net.sock")
    );
    assert!(spec.allow_net.is_empty());
    assert!(spec.secrets.is_empty());
    assert!(spec.ca_cert_pem.is_none());
    assert!(spec.ca_key_pem.is_none());
}

#[test]
fn factory_creates_backend_whose_spec_reflects_config() {
    // The abstract factory is used purely through the trait — the caller never
    // names a concrete backend. The one created backend produces the wire spec.
    use boxlite::net::{NetworkBackendFactory, default_factory};

    let factory: std::sync::Arc<dyn NetworkBackendFactory> = default_factory();
    let config = test_config(PathBuf::from("/tmp/factory-test/net.sock"));

    let backend = factory.create(&config).expect("gvproxy backend");
    let spec = backend.spec();
    // The config's socket crosses into the wire spec via production spec().
    assert_eq!(spec.socket_path, config.socket_path);
    // No secrets configured → no CA is minted.
    assert!(spec.ca_cert_pem.is_none());
}

#[test]
fn factory_backend_carries_allowlist_and_secret_metadata_in_spec() {
    use boxlite::net::{NetworkBackendFactory, default_factory};
    use boxlite::runtime::options::Secret;

    let ca_dir = tempfile::tempdir().unwrap();
    let mut config = test_config(PathBuf::from("/tmp/factory-allowlist/net.sock"));
    config.ca_dir = ca_dir.path().to_path_buf();
    config.allow_net = vec!["api.openai.com".to_string(), "example.com".to_string()];
    config.secrets = vec![Secret {
        name: "openai".to_string(),
        hosts: vec!["api.openai.com".to_string()],
        placeholder: "<BOXLITE_SECRET:openai>".to_string(),
        value: "sk-test-not-a-real-key".to_string(),
    }];

    let factory: std::sync::Arc<dyn NetworkBackendFactory> = default_factory();
    let backend = factory.create(&config).expect("gvproxy backend");
    let spec = backend.spec();
    assert_eq!(spec.allow_net, config.allow_net);
    assert_eq!(spec.secrets.len(), 1);
    assert_eq!(spec.secrets[0].name, "openai");
    assert_eq!(spec.secrets[0].hosts, vec!["api.openai.com"]);
    assert_eq!(spec.secrets[0].placeholder, "<BOXLITE_SECRET:openai>");
}

#[test]
fn factory_backend_with_secrets_mints_ca_in_spec() {
    // Public-API path: with secrets configured, the created backend's spec()
    // mints an ephemeral MITM CA into ca_dir — the create → spec-with-CA flow
    // the core relies on to hand the shim a usable CA.
    use boxlite::net::{NetworkBackendFactory, default_factory};
    use boxlite::runtime::options::Secret;

    let ca_dir = tempfile::tempdir().unwrap();
    let mut config = test_config(PathBuf::from("/tmp/factory-secrets/net.sock"));
    config.ca_dir = ca_dir.path().to_path_buf();
    config.secrets = vec![Secret {
        name: "openai".to_string(),
        hosts: vec!["api.openai.com".to_string()],
        placeholder: "<BOXLITE_SECRET:openai>".to_string(),
        value: "sk-test-not-a-real-key".to_string(),
    }];

    let factory: std::sync::Arc<dyn NetworkBackendFactory> = default_factory();
    let backend = factory.create(&config).expect("gvproxy backend");
    let spec = backend.spec();
    assert!(
        spec.ca_cert_pem
            .as_deref()
            .unwrap()
            .contains("BEGIN CERTIFICATE"),
        "secrets should mint a CA cert on the wire spec"
    );
    assert!(spec.ca_key_pem.is_some(), "CA key should be present");
    assert_eq!(spec.secrets.len(), 1, "secrets stay enabled with a CA");
}

#[test]
fn explicit_no_backend_factory_yields_none() {
    use boxlite::net::{NetworkBackendFactory, NoBackendFactory};

    let factory: std::sync::Arc<dyn NetworkBackendFactory> = std::sync::Arc::new(NoBackendFactory);
    let config = test_config(PathBuf::from("/tmp/factory-test/net.sock"));
    assert!(factory.create(&config).is_none());
}

#[tokio::test]
async fn factory_backend_control_dials_derived_gvproxy_socket() {
    use boxlite::net::{NetworkBackendFactory, default_factory};

    let dir = tempfile::Builder::new()
        .prefix("bl-factory-control-")
        .tempdir_in("/tmp")
        .unwrap();
    let config = test_config(dir.path().join("net.sock"));

    let factory: std::sync::Arc<dyn NetworkBackendFactory> = default_factory();
    let backend = factory.create(&config).expect("gvproxy backend");
    assert_eq!(backend.name(), "gvisor-tap-vsock");

    let err = backend.list_forwards().await.unwrap_err();

    let err = format!("{err}");
    assert!(err.contains("gvproxy services connect"), "err: {err}");
    assert!(err.contains("gvproxy-ctl.sock"), "err: {err}");
    assert!(
        !err.contains("does not support list_forwards"),
        "factory must create a runtime control backend, err: {err}"
    );
}
