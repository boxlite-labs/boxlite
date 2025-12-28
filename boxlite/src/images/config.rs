//! Container image configuration extracted from OCI images config

use serde::{Deserialize, Serialize};

/// Container image configuration extracted from OCI images.
///
/// This struct contains the configuration baked into the container image,
/// including entrypoint, environment variables, working directory, and
/// exposed ports.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContainerImageConfig {
    /// Entrypoint command (e.g., ["/bin/python", "-u", "app.py"])
    ///
    /// This combines the images's ENTRYPOINT and CMD directives.
    pub cmd: Vec<String>,

    /// Exposed ports from the images (e.g., ["8080/tcp", "443/tcp"])
    ///
    /// These are the ports declared in the images's EXPOSE directive.
    /// Format: "port/protocol" where protocol is "tcp" or "udp".
    pub exposed_ports: Vec<String>,

    /// Environment variables (e.g., ["PATH=/usr/bin", "HOME=/root"])
    pub env: Vec<String>,

    /// Working directory (e.g., "/app", "/workspace")
    pub working_dir: String,
}

impl ContainerImageConfig {
    /// Create a new ContainerImageConfig with defaults
    #[allow(dead_code)]
    pub fn new() -> Self {
        Self::default()
    }

    /// Parse port number and protocol from exposed port string
    ///
    /// # Examples
    /// - "8080/tcp" -> Some((8080, "tcp"))
    /// - "53/udp" -> Some((53, "udp"))
    /// - "8080" -> Some((8080)) // Default to TCP
    pub fn parse_exposed_port(port_spec: &str) -> Option<(u16, &str)> {
        let parts: Vec<&str> = port_spec.split('/').collect();

        let port_str = parts.first()?;
        let port: u16 = port_str.parse().ok()?;

        let protocol = parts.get(1).copied().unwrap_or("tcp");

        Some((port, protocol))
    }

    /// Get TCP ports from exposed ports
    pub fn tcp_ports(&self) -> Vec<u16> {
        self.exposed_ports
            .iter()
            .filter_map(|spec| {
                Self::parse_exposed_port(spec).and_then(|(port, protocol)| {
                    if protocol == "tcp" { Some(port) } else { None }
                })
            })
            .collect()
    }

    /// Get UDP ports from exposed ports
    #[allow(dead_code)]
    pub fn udp_ports(&self) -> Vec<u16> {
        self.exposed_ports
            .iter()
            .filter_map(|spec| {
                Self::parse_exposed_port(spec).and_then(|(port, protocol)| {
                    if protocol == "udp" { Some(port) } else { None }
                })
            })
            .collect()
    }

    /// Merge user-provided environment variables with images environment
    ///
    /// User env vars override images env vars if they have the same key.
    /// Input format is Vec<(key, value)>, output format is Vec<"KEY=VALUE">
    pub fn merge_env(&mut self, user_env: Vec<(String, String)>) {
        use std::collections::HashMap;

        // Parse existing env into map (KEY=VALUE)
        let mut env_map: HashMap<String, String> = HashMap::new();
        for entry in &self.env {
            if let Some(pos) = entry.find('=') {
                let key = entry[..pos].to_string();
                let value = entry[pos + 1..].to_string();
                env_map.insert(key, value);
            }
        }

        // Merge user env (overwrites existing keys)
        for (key, value) in user_env {
            env_map.insert(key, value);
        }

        // Convert back to Vec<String> in sorted order for determinism
        let mut env_vec: Vec<String> = env_map
            .into_iter()
            .map(|(k, v)| format!("{}={}", k, v))
            .collect();
        env_vec.sort();

        self.env = env_vec;
    }

    /// Convert OCI ImageConfiguration to ContainerImageConfig
    ///
    /// Extracts container runtime configuration from OCI images config,
    /// including entrypoint (combined ENTRYPOINT + CMD), environment variables,
    /// working directory, and exposed ports.
    ///
    /// # Arguments
    /// * `image_config` - OCI ImageConfiguration from images config.json
    ///
    /// # Returns
    /// ContainerImageConfig with extracted image configuration
    pub fn from_oci_config(
        image_config: &oci_spec::image::ImageConfiguration,
    ) -> boxlite_shared::errors::BoxliteResult<Self> {
        use boxlite_shared::errors::BoxliteError;

        let config = image_config.config().as_ref().ok_or_else(|| {
            BoxliteError::Storage("Config object missing from images config".into())
        })?;

        // Build entrypoint: combine Entrypoint + Cmd
        let mut entrypoint = Vec::new();
        if let Some(ep) = config.entrypoint().as_ref() {
            entrypoint.extend(ep.iter().cloned());
        }
        if let Some(cmd) = config.cmd().as_ref() {
            entrypoint.extend(cmd.iter().cloned());
        }

        // Extract environment variables
        let env = config.env().clone().unwrap_or_default();

        // Extract working directory
        let workdir = config
            .working_dir()
            .as_ref()
            .map(|s| s.to_string())
            .unwrap_or_else(|| "/".to_string());

        // Extract exposed ports
        let exposed_ports = config.exposed_ports().clone().unwrap_or_default();

        Ok(ContainerImageConfig {
            cmd: entrypoint,
            env,
            working_dir: workdir,
            exposed_ports,
        })
    }
}

impl Default for ContainerImageConfig {
    fn default() -> Self {
        Self {
            cmd: vec!["/bin/sh".to_string()],
            env: vec![
                "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".to_string(),
            ],
            working_dir: "/".to_string(),
            exposed_ports: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_exposed_port() {
        assert_eq!(
            ContainerImageConfig::parse_exposed_port("8080/tcp"),
            Some((8080, "tcp"))
        );
        assert_eq!(
            ContainerImageConfig::parse_exposed_port("53/udp"),
            Some((53, "udp"))
        );
        assert_eq!(
            ContainerImageConfig::parse_exposed_port("8080"),
            Some((8080, "tcp"))
        );
        assert_eq!(ContainerImageConfig::parse_exposed_port("invalid"), None);
    }

    #[test]
    fn test_tcp_ports() {
        let config = ContainerImageConfig {
            cmd: vec![],
            env: vec![],
            working_dir: "/".to_string(),
            exposed_ports: vec![
                "8080/tcp".to_string(),
                "443/tcp".to_string(),
                "53/udp".to_string(),
            ],
        };

        assert_eq!(config.tcp_ports(), vec![8080, 443]);
    }

    #[test]
    fn test_udp_ports() {
        let config = ContainerImageConfig {
            cmd: vec![],
            env: vec![],
            working_dir: "/".to_string(),
            exposed_ports: vec![
                "8080/tcp".to_string(),
                "53/udp".to_string(),
                "123/udp".to_string(),
            ],
        };

        assert_eq!(config.udp_ports(), vec![53, 123]);
    }
}
