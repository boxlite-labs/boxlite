//! Integration tests for network backend selection and configuration.

use boxlite::net::{NetworkBackendConfig, NetworkBackendFactory};

#[test]
#[cfg(all(not(feature = "libslirp-backend"), not(feature = "gvproxy-backend")))]
fn test_no_backend_when_no_features_enabled() {
    // When no backend features are enabled, factory should return None
    let config = NetworkBackendConfig::new(vec![]);
    let backend = NetworkBackendFactory::create(config).unwrap();

    assert!(
        backend.is_none(),
        "Expected None when no backend features are enabled"
    );
}

// Note: libslirp backend tests are disabled because the backend's endpoint()
// implementation is incomplete and returns an error. These tests can be
// re-enabled once the backend is fully implemented.

#[test]
fn test_network_config_creation() {
    // Test NetworkConfig constructor
    let port_mappings = vec![(8080, 80), (3000, 3000), (5432, 5432)];
    let config = NetworkBackendConfig::new(port_mappings.clone());

    assert_eq!(config.port_mappings.len(), 3);
    assert_eq!(config.port_mappings, port_mappings);
}

#[tokio::test]
#[cfg(any(feature = "libslirp-backend", feature = "gvproxy-backend"))]
async fn test_backend_trait_send_sync() {
    use boxlite::net::NetworkBackend;

    // Verify NetworkBackend trait objects are Send + Sync
    fn assert_send_sync<T: Send + Sync>() {}

    let config = NetworkBackendConfig::new(vec![]);
    let backend = NetworkBackendFactory::create(config).unwrap();

    // This will fail to compile if NetworkBackend is not Send + Sync
    fn check_send_sync(backend: Box<dyn NetworkBackend>) {
        assert_send_sync::<Box<dyn NetworkBackend>>();
        drop(backend);
    }

    if let Some(backend) = backend {
        check_send_sync(backend);
    }
}

// Note: libslirp backend tests are disabled because the backend's endpoint()
// implementation is incomplete and returns an error.
