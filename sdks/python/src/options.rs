use std::path::PathBuf;

use boxlite::BoxliteRestOptions;
use boxlite::litebox::copy::CopyOptions;
use boxlite::runtime::advanced_options::{HealthCheckOptions, SecurityOptions};
use boxlite::runtime::constants::images;
use boxlite::runtime::options::{
    BoxOptions, BoxliteOptions, ImageRegistry, ImageRegistryAuth, InboundNetworkConfig,
    NetworkMode, NetworkSpec, OutboundNetworkConfig, PortProtocol, PortSpec, RegistryTransport,
    RootfsSpec, VolumeSpec,
};
use pyo3::exceptions::{PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyAnyMethods, PyDict, PyTuple};

use crate::advanced_options::PyAdvancedBoxOptions;

#[pyclass(name = "Options")]
#[derive(Clone, Debug)]
pub(crate) struct PyOptions {
    #[pyo3(get, set)]
    pub(crate) home_dir: Option<String>,
    /// Registry transport, TLS, search, and auth configuration.
    #[pyo3(get, set)]
    pub(crate) image_registries: Vec<PyImageRegistry>,
}

#[pymethods]
impl PyOptions {
    #[new]
    #[pyo3(signature = (home_dir=None, image_registries=vec![]))]
    fn new(home_dir: Option<String>, image_registries: Vec<PyImageRegistry>) -> Self {
        Self {
            home_dir,
            image_registries,
        }
    }

    fn __repr__(&self) -> String {
        format!(
            "Options(home_dir={:?}, image_registries={:?})",
            self.home_dir, self.image_registries
        )
    }
}

impl PyOptions {
    pub(crate) fn into_core(self) -> PyResult<BoxliteOptions> {
        let mut config = BoxliteOptions::default();

        if let Some(home_dir) = self.home_dir {
            config.home_dir = PathBuf::from(home_dir);
        }

        config.image_registries = self
            .image_registries
            .into_iter()
            .map(PyImageRegistry::into_core)
            .collect::<PyResult<Vec<_>>>()?;

        Ok(config)
    }
}

#[pyclass(name = "ImageRegistry")]
#[derive(Clone)]
pub(crate) struct PyImageRegistry {
    #[pyo3(get, set)]
    pub(crate) host: String,
    #[pyo3(get, set)]
    pub(crate) transport: String,
    #[pyo3(get, set)]
    pub(crate) skip_verify: bool,
    #[pyo3(get, set)]
    pub(crate) search: bool,
    #[pyo3(get, set)]
    pub(crate) username: Option<String>,
    #[pyo3(get, set)]
    pub(crate) password: Option<String>,
    #[pyo3(get, set)]
    pub(crate) bearer_token: Option<String>,
}

impl std::fmt::Debug for PyImageRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImageRegistry")
            .field("host", &self.host)
            .field("transport", &self.transport)
            .field("skip_verify", &self.skip_verify)
            .field("search", &self.search)
            .field("username", &self.username)
            .field("password", &self.password.as_ref().map(|_| "***"))
            .field("bearer_token", &self.bearer_token.as_ref().map(|_| "***"))
            .finish()
    }
}

#[pymethods]
impl PyImageRegistry {
    #[new]
    #[pyo3(signature = (
        host,
        transport = "https".to_string(),
        skip_verify = false,
        search = false,
        username = None,
        password = None,
        bearer_token = None
    ))]
    fn new(
        host: String,
        transport: String,
        skip_verify: bool,
        search: bool,
        username: Option<String>,
        password: Option<String>,
        bearer_token: Option<String>,
    ) -> PyResult<Self> {
        validate_registry_host(&host)?;
        parse_registry_transport(&transport)?;
        validate_registry_auth(&username, &password)?;

        Ok(Self {
            host,
            transport,
            skip_verify,
            search,
            username,
            password,
            bearer_token,
        })
    }

    fn __repr__(&self) -> String {
        format!(
            "ImageRegistry(host={:?}, transport={:?}, skip_verify={}, search={})",
            self.host, self.transport, self.skip_verify, self.search
        )
    }
}

impl PyImageRegistry {
    fn into_core(self) -> PyResult<ImageRegistry> {
        validate_registry_host(&self.host)?;
        let transport = parse_registry_transport(&self.transport)?;
        validate_registry_auth(&self.username, &self.password)?;

        let auth = if let Some(token) = self.bearer_token {
            ImageRegistryAuth::Bearer { token }
        } else if let (Some(username), Some(password)) = (self.username, self.password) {
            ImageRegistryAuth::Basic { username, password }
        } else {
            ImageRegistryAuth::Anonymous
        };

        Ok(ImageRegistry {
            host: self.host,
            transport,
            skip_verify: self.skip_verify,
            search: self.search,
            auth,
        })
    }
}

fn validate_registry_host(host: &str) -> PyResult<()> {
    if host.trim().is_empty() {
        return Err(PyRuntimeError::new_err("image registry host is required"));
    }
    if host.contains("://") || host.contains('/') {
        return Err(PyRuntimeError::new_err(format!(
            "image registry host must be host[:port], not a URL: {host}"
        )));
    }
    Ok(())
}

fn parse_registry_transport(transport: &str) -> PyResult<RegistryTransport> {
    match transport {
        "" | "https" => Ok(RegistryTransport::Https),
        "http" => Ok(RegistryTransport::Http),
        _ => Err(PyRuntimeError::new_err(format!(
            "unsupported registry transport: {transport}"
        ))),
    }
}

fn validate_registry_auth(username: &Option<String>, password: &Option<String>) -> PyResult<()> {
    if username.is_some() != password.is_some() {
        return Err(PyRuntimeError::new_err(
            "registry username and password must be provided together",
        ));
    }
    Ok(())
}

// ============================================================================
// Copy Options
// ============================================================================

#[pyclass(name = "CopyOptions")]
#[derive(Clone, Debug)]
pub struct PyCopyOptions {
    #[pyo3(get, set)]
    pub recursive: bool,
    #[pyo3(get, set)]
    pub overwrite: bool,
    #[pyo3(get, set)]
    pub follow_symlinks: bool,
    #[pyo3(get, set)]
    pub include_parent: bool,
}

#[pymethods]
impl PyCopyOptions {
    #[new]
    #[pyo3(
        signature = (
            recursive = true,
            overwrite = true,
            follow_symlinks = false,
            include_parent = true
        )
    )]
    fn new(recursive: bool, overwrite: bool, follow_symlinks: bool, include_parent: bool) -> Self {
        Self {
            recursive,
            overwrite,
            follow_symlinks,
            include_parent,
        }
    }
}

impl From<PyCopyOptions> for CopyOptions {
    fn from(opt: PyCopyOptions) -> Self {
        Self {
            recursive: opt.recursive,
            overwrite: opt.overwrite,
            follow_symlinks: opt.follow_symlinks,
            include_parent: opt.include_parent,
        }
    }
}

// ============================================================================
// NetworkSpec
// ============================================================================

/// Network policy for a box.
///
/// Prefer the nested `outbound` and `inbound` fields. The constructor also
/// accepts legacy `mode` and `allow_net` keywords as a compatibility shim.
#[pyclass(name = "NetworkSpec")]
#[derive(Clone, Debug)]
pub(crate) struct PyNetworkSpec {
    #[pyo3(get, set)]
    pub(crate) outbound: Option<PyOutboundNetworkSpec>,
    #[pyo3(get, set)]
    pub(crate) inbound: Option<PyInboundNetworkSpec>,
}

/// Outbound network policy.
///
/// `mode` accepts `"enabled"` or `"disabled"`. `allow_net` contains the host
/// patterns allowed when outbound networking is enabled.
#[pyclass(name = "OutboundNetworkSpec")]
#[derive(Clone, Debug)]
pub(crate) struct PyOutboundNetworkSpec {
    #[pyo3(get, set)]
    pub(crate) mode: String,
    #[pyo3(get, set)]
    pub(crate) allow_net: Vec<String>,
}

/// Inbound network policy.
///
/// Aligned field-for-field with `NetworkSpec`: `mode` accepts
/// `"enabled"` (services the box exposes are publicly reachable) or
/// `"disabled"` (private). `allow_net` exists for shape symmetry only — no
/// layer enforces an inbound allowlist yet, so a non-empty value is
/// rejected.
#[pyclass(name = "InboundNetworkSpec")]
#[derive(Clone, Debug)]
pub(crate) struct PyInboundNetworkSpec {
    #[pyo3(get, set)]
    pub(crate) mode: String,
    #[pyo3(get, set)]
    pub(crate) allow_net: Vec<String>,
}

#[pymethods]
impl PyNetworkSpec {
    /// Create a network policy from nested specs or legacy outbound keywords.
    ///
    /// Passing `outbound` together with legacy `mode` or `allow_net` raises
    /// `ValueError`; callers should use one shape per request.
    #[new]
    #[pyo3(signature = (outbound=None, inbound=None, mode=None, allow_net=None))]
    fn new(
        outbound: Option<&Bound<'_, PyAny>>,
        inbound: Option<&Bound<'_, PyAny>>,
        mode: Option<String>,
        allow_net: Option<Vec<String>>,
    ) -> PyResult<Self> {
        // The pre-split signature was `NetworkSpec(mode, allow_net)`, so the
        // first two positional slots used to hold a `str` and a list of `str`.
        // Both legacy shapes stay callable because each is unambiguous by
        // type: a `str` in slot 1 is a mode, a sequence in slot 2 is an
        // allowlist. Anything else must be the nested spec objects.
        let (outbound, positional_mode) = match outbound {
            None => (None, None),
            Some(value) => match value.extract::<String>() {
                Ok(legacy_mode) => (None, Some(legacy_mode)),
                Err(_) => (Some(value.extract::<PyOutboundNetworkSpec>()?), None),
            },
        };
        let (inbound, positional_allow_net) = match inbound {
            None => (None, None),
            Some(value) => match value.extract::<Vec<String>>() {
                Ok(legacy_allow_net) => (None, Some(legacy_allow_net)),
                Err(_) => (Some(value.extract::<PyInboundNetworkSpec>()?), None),
            },
        };

        let mode = match (positional_mode, mode) {
            (Some(_), Some(_)) => {
                return Err(PyValueError::new_err(
                    "NetworkSpec got mode both positionally and by keyword",
                ));
            }
            (positional, keyword) => positional.or(keyword),
        };
        let allow_net = match (positional_allow_net, allow_net) {
            (Some(_), Some(_)) => {
                return Err(PyValueError::new_err(
                    "NetworkSpec got allow_net both positionally and by keyword",
                ));
            }
            (positional, keyword) => positional.or(keyword),
        };

        let legacy_outbound = if mode.is_some() || allow_net.is_some() {
            if outbound.is_some() {
                return Err(PyValueError::new_err(
                    "NetworkSpec cannot mix outbound with legacy mode/allow_net",
                ));
            }
            Some(PyOutboundNetworkSpec {
                mode: mode.unwrap_or_else(|| "enabled".to_string()),
                allow_net: allow_net.unwrap_or_default(),
            })
        } else {
            None
        };

        Ok(Self {
            outbound: outbound.or(legacy_outbound),
            inbound,
        })
    }

    /// Legacy view of the outbound mode.
    ///
    /// Deprecated: read `spec.outbound.mode`. Kept so pre-split readers keep
    /// working; defaults to `"enabled"` when no outbound policy is set, which
    /// is what an unset spec meant before the split.
    #[getter]
    fn mode(&self) -> String {
        self.outbound
            .as_ref()
            .map(|outbound| outbound.mode.clone())
            .unwrap_or_else(|| "enabled".to_string())
    }

    /// Legacy view of the outbound allowlist.
    ///
    /// Deprecated: read `spec.outbound.allow_net`.
    #[getter]
    fn allow_net(&self) -> Vec<String> {
        self.outbound
            .as_ref()
            .map(|outbound| outbound.allow_net.clone())
            .unwrap_or_default()
    }

    fn __repr__(&self) -> String {
        format!(
            "NetworkSpec(outbound={:?}, inbound={:?})",
            self.outbound, self.inbound
        )
    }
}

#[pymethods]
impl PyOutboundNetworkSpec {
    /// Create an outbound network policy.
    ///
    /// `mode` defaults to `"enabled"` and `allow_net` defaults to an empty
    /// allow list, which means no additional host allow-list restriction.
    #[new]
    #[pyo3(signature = (mode="enabled".to_string(), allow_net=vec![]))]
    fn new(mode: String, allow_net: Vec<String>) -> Self {
        Self { mode, allow_net }
    }
}

#[pymethods]
impl PyInboundNetworkSpec {
    /// Create an inbound network policy.
    ///
    /// `mode` defaults to `"enabled"` (publicly reachable) and `allow_net`
    /// defaults to an empty allow list, meaning no host-based restriction.
    #[new]
    #[pyo3(signature = (mode="enabled".to_string(), allow_net=vec![]))]
    fn new(mode: String, allow_net: Vec<String>) -> Self {
        Self { mode, allow_net }
    }
}

/// Both directions of the Python-facing network spec, converted into the two
/// independent `BoxOptions` fields.
impl TryFrom<PyNetworkSpec> for (NetworkSpec, NetworkSpec) {
    type Error = boxlite::BoxliteError;

    fn try_from(py_spec: PyNetworkSpec) -> Result<Self, Self::Error> {
        let PyNetworkSpec { outbound, inbound } = py_spec;

        let outbound = match outbound {
            Some(outbound) => outbound.try_into()?,
            None => NetworkSpec::default(),
        };
        let inbound = match inbound {
            Some(inbound) => NetworkSpec::try_from(InboundNetworkConfig {
                mode: inbound.mode.parse::<NetworkMode>()?,
                allow_net: inbound.allow_net,
            })?,
            None => NetworkSpec::default(),
        };
        Ok((outbound, inbound))
    }
}

impl TryFrom<PyOutboundNetworkSpec> for NetworkSpec {
    type Error = boxlite::BoxliteError;

    fn try_from(py_spec: PyOutboundNetworkSpec) -> Result<Self, Self::Error> {
        NetworkSpec::try_from(OutboundNetworkConfig {
            mode: py_spec.mode.parse::<NetworkMode>()?,
            allow_net: py_spec.allow_net,
        })
    }
}

// ============================================================================
// Secret
// ============================================================================

/// A secret to inject into outbound HTTPS requests via MITM proxy.
///
/// The guest code uses a placeholder string (e.g., ``<BOXLITE_SECRET:openai>``)
/// in HTTP headers. The host-side proxy replaces the placeholder with the
/// real secret value before forwarding the request. The actual secret never
/// enters the guest VM.
///
/// Example::
///
///     from boxlite import Secret
///
///     secret = Secret(
///         name="openai",
///         value="sk-...",
///         hosts=["api.openai.com"],
///     )
///     # Pass to BoxOptions:
///     opts = BoxOptions(image="python:3.12", secrets=[secret])
///
#[pyclass(name = "Secret")]
#[derive(Clone, Debug)]
pub(crate) struct PySecret {
    /// Human-readable name for the secret (e.g., "openai").
    #[pyo3(get, set)]
    pub(crate) name: String,

    /// The real secret value (never sent to the guest).
    #[pyo3(get, set)]
    pub(crate) value: String,

    /// Hostnames where this secret should be injected.
    /// Supports exact matches ("api.openai.com") and wildcards ("*.openai.com").
    #[pyo3(get, set)]
    pub(crate) hosts: Vec<String>,

    /// The placeholder string that appears in guest HTTP headers.
    /// When not supplied at construction time, defaults to
    /// ``<BOXLITE_SECRET:{name}>``. Always holds the effective value;
    /// never ``None``.
    #[pyo3(get, set)]
    pub(crate) placeholder: String,
}

#[pymethods]
impl PySecret {
    #[new]
    #[pyo3(signature = (name, value, hosts=vec![], placeholder=None))]
    fn new(name: String, value: String, hosts: Vec<String>, placeholder: Option<String>) -> Self {
        let placeholder = placeholder.unwrap_or_else(|| format!("<BOXLITE_SECRET:{}>", name));
        Self {
            name,
            value,
            hosts,
            placeholder,
        }
    }

    /// Return the effective placeholder string.
    ///
    /// Equivalent to reading ``.placeholder`` directly; kept for
    /// backwards-compatibility with code that called ``get_placeholder()``
    /// before ``.placeholder`` was guaranteed to hold the effective value.
    fn get_placeholder(&self) -> String {
        self.placeholder.clone()
    }

    fn __repr__(&self) -> String {
        format!(
            "Secret(name={:?}, hosts={:?}, placeholder={:?}, value=[REDACTED])",
            self.name,
            self.hosts,
            self.placeholder,
        )
    }
}

// ============================================================================
// Box Options
// ============================================================================

#[pyclass(name = "BoxOptions")]
#[derive(Clone, Debug)]
pub(crate) struct PyBoxOptions {
    #[pyo3(get, set)]
    pub(crate) image: Option<String>,
    #[pyo3(get, set)]
    pub(crate) rootfs_path: Option<String>,
    #[pyo3(get, set)]
    pub(crate) cpus: Option<u8>,
    #[pyo3(get, set)]
    pub(crate) memory_mib: Option<u32>,
    #[pyo3(get, set)]
    pub(crate) disk_size_gb: Option<u64>,
    #[pyo3(get, set)]
    pub(crate) working_dir: Option<String>,
    #[pyo3(get, set)]
    pub(crate) env: Vec<(String, String)>,
    pub(crate) volumes: Vec<PyVolumeSpec>,
    #[pyo3(get, set)]
    pub(crate) network: Option<PyNetworkSpec>,
    pub(crate) ports: Vec<PyPortSpec>,
    /// Deprecated compatibility option; use auto_delete.
    #[pyo3(get, set)]
    pub(crate) auto_remove: Option<bool>,
    #[pyo3(get, set)]
    pub(crate) auto_stop: Option<u32>,
    #[pyo3(get, set)]
    pub(crate) auto_delete: Option<u32>,
    #[pyo3(get, set)]
    pub(crate) auto_resume: Option<bool>,
    #[pyo3(get, set)]
    pub(crate) detach: Option<bool>,
    /// Override the image's ENTRYPOINT directive.
    /// When set, completely replaces the image's ENTRYPOINT.
    /// Example: `entrypoint=["dockerd"]` with `docker:dind`
    #[pyo3(get, set)]
    pub(crate) entrypoint: Option<Vec<String>>,
    /// Override the image's CMD. ENTRYPOINT is preserved.
    /// Example: `cmd=["--iptables=false"]` with `docker:dind`
    #[pyo3(get, set)]
    pub(crate) cmd: Option<Vec<String>>,
    /// Username or UID (format: <name|uid>[:<group|gid>]).
    /// If None, uses the image's USER directive (defaults to root).
    #[pyo3(get, set)]
    pub(crate) user: Option<String>,

    /// Run the box's main command on a terminal (docker `run -t`).
    ///
    /// A property of the box, not of an attach: the main command is the
    /// container's init, so whether it gets a terminal is fixed at create time.
    #[pyo3(get, set)]
    pub(crate) tty: Option<bool>,

    /// Advanced options for expert users (capabilities, security, mount isolation, health check).
    #[pyo3(get, set)]
    pub(crate) advanced: Option<PyAdvancedBoxOptions>,

    /// Secrets to inject into outbound HTTPS requests via MITM proxy.
    #[pyo3(get, set)]
    pub(crate) secrets: Vec<PySecret>,
}

#[pymethods]
impl PyBoxOptions {
    #[new]
    #[pyo3(signature = (
        image=None,
        rootfs_path=None,
        cpus=None,
        memory_mib=None,
        disk_size_gb=None,
        working_dir=None,
        env=vec![],
        volumes=vec![],
        network=None,
        ports=vec![],
        auto_remove=None,
        auto_stop=None,
        auto_delete=None,
        auto_resume=None,
        detach=None,
        entrypoint=None,
        cmd=None,
        user=None,
        tty=None,
        advanced=None,
        secrets=vec![],
    ))]
    #[allow(clippy::too_many_arguments)]
    fn new(
        image: Option<String>,
        rootfs_path: Option<String>,
        cpus: Option<u8>,
        memory_mib: Option<u32>,
        disk_size_gb: Option<u64>,
        working_dir: Option<String>,
        env: Vec<(String, String)>,
        volumes: Vec<PyVolumeSpec>,
        network: Option<PyNetworkSpec>,
        ports: Vec<PyPortSpec>,
        auto_remove: Option<bool>,
        auto_stop: Option<u32>,
        auto_delete: Option<u32>,
        auto_resume: Option<bool>,
        detach: Option<bool>,
        entrypoint: Option<Vec<String>>,
        cmd: Option<Vec<String>>,
        user: Option<String>,
        tty: Option<bool>,
        advanced: Option<PyAdvancedBoxOptions>,
        secrets: Vec<PySecret>,
    ) -> Self {
        Self {
            image,
            rootfs_path,
            cpus,
            memory_mib,
            disk_size_gb,
            working_dir,
            env,
            volumes,
            network,
            ports,
            auto_remove,
            auto_stop,
            auto_delete,
            auto_resume,
            detach,
            entrypoint,
            cmd,
            user,
            tty,
            advanced,
            secrets,
        }
    }

    fn __repr__(&self) -> String {
        format!(
            "BoxOptions(image={:?}, rootfs_path={:?}, cpus={:?}, memory_mib={:?}, advanced={:?})",
            self.image,
            self.rootfs_path,
            self.cpus,
            self.memory_mib,
            self.advanced.is_some()
        )
    }
}

impl TryFrom<PyBoxOptions> for BoxOptions {
    type Error = boxlite::BoxliteError;

    #[allow(deprecated)]
    fn try_from(py_opts: PyBoxOptions) -> Result<Self, Self::Error> {
        let auto_remove = py_opts.auto_remove;
        let auto_delete = py_opts.auto_delete;
        let volumes = py_opts.volumes.into_iter().map(VolumeSpec::from).collect();

        let (network, inbound_network) = match py_opts.network {
            Some(spec) => <(NetworkSpec, NetworkSpec)>::try_from(spec)?,
            None => (NetworkSpec::default(), NetworkSpec::default()),
        };

        let ports = py_opts.ports.into_iter().map(PortSpec::from).collect();

        // Convert image/rootfs_path to RootfsSpec
        let rootfs = match &py_opts.rootfs_path {
            Some(path) if !path.is_empty() => RootfsSpec::RootfsPath(path.clone()),
            _ => {
                let image = py_opts
                    .image
                    .clone()
                    .unwrap_or_else(|| images::DEFAULT.to_string());
                RootfsSpec::Image(image)
            }
        };

        let mut opts = BoxOptions {
            cpus: py_opts.cpus,
            memory_mib: py_opts.memory_mib,
            disk_size_gb: py_opts.disk_size_gb,
            working_dir: py_opts.working_dir,
            env: py_opts.env,
            rootfs,
            volumes,
            network,
            inbound_network,
            ports,
            auto_stop: py_opts.auto_stop,
            auto_delete,
            auto_resume: py_opts.auto_resume,
            entrypoint: py_opts.entrypoint,
            cmd: py_opts.cmd,
            user: py_opts.user,
            ..Default::default()
        };

        // These core fields have concrete defaults. `None` means keep the default.
        if let Some(auto_remove) = auto_remove {
            opts.auto_remove = auto_remove;
        }

        if let Some(detach) = py_opts.detach {
            opts.detach = detach;
        }

        if let Some(tty) = py_opts.tty {
            opts.tty = tty;
        }

        if let Some(advanced) = py_opts.advanced {
            if let Some(security) = advanced.security {
                opts.advanced.security = SecurityOptions::from(security);
            }
            if let Some(health_check) = advanced.health_check {
                opts.advanced.health_check = Some(HealthCheckOptions::from(health_check));
            }
            if let Some(capabilities) = advanced.capabilities {
                opts.advanced.set_capabilities(Some(capabilities.into()))?;
            }
        }

        // Convert Python secrets to Rust secrets. `s.placeholder` is always
        // the effective value (computed eagerly in PySecret::new).
        opts.secrets = py_opts
            .secrets
            .into_iter()
            .map(|s| boxlite::runtime::options::Secret {
                name: s.name,
                hosts: s.hosts,
                placeholder: s.placeholder,
                value: s.value,
            })
            .collect();

        Ok(opts)
    }
}

/// One entry of the `volumes=` argument, before it becomes a [`VolumeSpec`].
///
/// `managed_volume` holds a volume id or name from the `managed_volume` key;
/// `host_path` holds a bind path from a tuple or the `host_path` key. Exactly
/// one is set. The dict keys are the core field names verbatim, so one
/// vocabulary spans Python, the Rust core and the wire.
#[derive(Clone, Debug)]
pub(crate) struct PyVolumeSpec {
    managed_volume: Option<String>,
    host_path: String,
    guest_path: String,
    read_only: bool,
}

impl From<PyVolumeSpec> for VolumeSpec {
    fn from(v: PyVolumeSpec) -> Self {
        let spec = match v.managed_volume {
            Some(managed_volume) => VolumeSpec::managed_volume(managed_volume, v.guest_path),
            None => VolumeSpec::bind_mount(v.host_path, v.guest_path),
        };

        VolumeSpec {
            read_only: v.read_only,
            ..spec
        }
    }
}

impl<'a, 'py> pyo3::FromPyObject<'a, 'py> for PyVolumeSpec {
    type Error = PyErr;

    fn extract(ob: Borrowed<'a, 'py, PyAny>) -> PyResult<Self> {
        let obj = ob.to_owned();

        // Tuple form is a host bind, and only a host bind: it predates managed
        // volumes and has no slot to say which origin it means.
        if let Ok(t) = obj.cast::<PyTuple>() {
            let len = t.len();
            let err = || {
                PyRuntimeError::new_err(
                    "volumes tuples must be (host_path, guest_path[, read_only])",
                )
            };
            let host_path: String;
            let guest_path: String;
            let read_only: bool;

            match len {
                2 => {
                    host_path = t.get_item(0)?.extract()?;
                    guest_path = t.get_item(1)?.extract()?;
                    read_only = false;
                }
                3 => {
                    host_path = t.get_item(0)?.extract()?;
                    guest_path = t.get_item(1)?.extract()?;
                    read_only = t.get_item(2)?.extract()?;
                }
                _ => return Err(err()),
            }

            return Ok(PyVolumeSpec {
                managed_volume: None,
                host_path,
                guest_path,
                read_only,
            });
        }

        if let Ok(d) = obj.cast::<PyDict>() {
            // Unknown keys are an error, not noise. `ro` and `guest` used to be
            // accepted aliases; ignoring them now would silently hand back a
            // read-write mount to a caller who asked for read-only.
            const KEYS: [&str; 4] = ["managed_volume", "host_path", "guest_path", "read_only"];
            for key in d.keys() {
                let key: String = key.extract()?;
                if !KEYS.contains(&key.as_str()) {
                    return Err(PyRuntimeError::new_err(format!(
                        "unknown volume dict key {key:?}; expected one of {}",
                        KEYS.join(", ")
                    )));
                }
            }

            let managed_volume: Option<String> = match d.get_item("managed_volume") {
                Ok(Some(v)) => Some(v.extract()?),
                _ => None,
            };
            let host_path: Option<String> = match d.get_item("host_path") {
                Ok(Some(v)) => Some(v.extract()?),
                _ => None,
            };

            let (managed_volume, host_path) = match (managed_volume, host_path) {
                (Some(_), Some(_)) => {
                    return Err(PyRuntimeError::new_err(
                        "volume dict takes managed_volume or host_path, not both",
                    ));
                }
                (Some(managed_volume), None) => (Some(managed_volume), String::new()),
                (None, Some(host_path)) => (None, host_path),
                (None, None) => {
                    return Err(PyRuntimeError::new_err(
                        "volume dict requires managed_volume or host_path",
                    ));
                }
            };

            let guest_path: String = match d.get_item("guest_path") {
                Ok(Some(v)) => v.extract()?,
                _ => return Err(PyRuntimeError::new_err("volume dict missing guest_path")),
            };

            let read_only: bool = match d.get_item("read_only") {
                Ok(Some(v)) => v.extract()?,
                _ => false,
            };

            return Ok(PyVolumeSpec {
                managed_volume,
                host_path,
                guest_path,
                read_only,
            });
        }

        Err(PyRuntimeError::new_err(
            "volumes entries must be tuple or dict",
        ))
    }
}

#[derive(Clone, Debug)]
pub(crate) struct PyPortSpec {
    host: Option<u16>,
    guest: u16,
    protocol: PortProtocol,
    host_ip: Option<String>,
}

impl From<PyPortSpec> for PortSpec {
    fn from(p: PyPortSpec) -> Self {
        PortSpec {
            host_port: p.host,
            guest_port: p.guest,
            protocol: p.protocol,
            host_ip: p.host_ip,
        }
    }
}

impl<'a, 'py> pyo3::FromPyObject<'a, 'py> for PyPortSpec {
    type Error = PyErr;

    fn extract(ob: Borrowed<'a, 'py, PyAny>) -> PyResult<Self> {
        let obj = ob.to_owned();

        if let Ok(t) = obj.cast::<PyTuple>() {
            let len = t.len();
            let err = || {
                PyRuntimeError::new_err("ports tuples must be (host, guest[, protocol[, host_ip]])")
            };
            let host_port: Option<u16>;
            let guest_port: u16;
            let protocol: Option<String>;
            let host_ip: Option<String>;

            match len {
                2 => {
                    host_port = Some(t.get_item(0)?.extract()?);
                    guest_port = t.get_item(1)?.extract()?;
                    protocol = None;
                    host_ip = None;
                }
                3 => {
                    host_port = Some(t.get_item(0)?.extract()?);
                    guest_port = t.get_item(1)?.extract()?;
                    protocol = Some(t.get_item(2)?.extract()?);
                    host_ip = None;
                }
                4 => {
                    host_port = Some(t.get_item(0)?.extract()?);
                    guest_port = t.get_item(1)?.extract()?;
                    protocol = Some(t.get_item(2)?.extract()?);
                    host_ip = Some(t.get_item(3)?.extract()?);
                }
                _ => return Err(err()),
            }

            return Ok(PyPortSpec {
                host: host_port,
                guest: guest_port,
                protocol: parse_protocol(protocol.as_deref().unwrap_or("tcp")),
                host_ip: host_ip.filter(|s| !s.is_empty()),
            });
        }

        if let Ok(d) = obj.cast::<PyDict>() {
            let guest_port: u16 = if let Ok(Some(v)) = d.get_item("guest_port") {
                v.extract()?
            } else if let Ok(Some(v)) = d.get_item("guest") {
                v.extract()?
            } else {
                return Err(PyRuntimeError::new_err("ports dict missing guest_port"));
            };

            let host_port: Option<u16> = if let Ok(Some(v)) = d.get_item("host_port") {
                Some(v.extract()?)
            } else if let Ok(Some(v)) = d.get_item("host") {
                Some(v.extract()?)
            } else {
                None
            };

            let protocol: Option<String> = if let Ok(Some(v)) = d.get_item("protocol") {
                Some(v.extract()?)
            } else {
                None
            };

            let host_ip: Option<String> = if let Ok(Some(v)) = d.get_item("host_ip") {
                Some(v.extract()?)
            } else {
                None
            };

            return Ok(PyPortSpec {
                host: host_port,
                guest: guest_port,
                protocol: parse_protocol(protocol.as_deref().unwrap_or("tcp")),
                host_ip: host_ip.filter(|s| !s.is_empty()),
            });
        }

        Err(PyRuntimeError::new_err(
            "ports entries must be tuple or dict",
        ))
    }
}

fn parse_protocol<S: AsRef<str>>(s: S) -> PortProtocol {
    match s.as_ref().to_ascii_lowercase().as_str() {
        "udp" => PortProtocol::Udp,
        // "sctp" => PortProtocol::Sctp,
        _ => PortProtocol::Tcp,
    }
}

// ============================================================================
// REST Options
// ============================================================================

/// A bearer token plus its expiry. Mirrors the Rust `AccessToken`.
/// `expires_at` is epoch seconds, or `None` for non-expiring tokens
/// (e.g. API keys). The token string is masked in `repr()`.
#[pyclass(name = "AccessToken")]
#[derive(Clone)]
pub(crate) struct PyAccessToken {
    #[pyo3(get)]
    token: String,
    #[pyo3(get)]
    expires_at: Option<f64>,
}

#[pymethods]
impl PyAccessToken {
    fn __repr__(&self) -> String {
        format!("AccessToken(token='***', expires_at={:?})", self.expires_at)
    }
}

/// Long-lived opaque API key credential.
///
/// Concrete implementation of the `Credential` ABC (see
/// ``boxlite.credential``). Registered as a virtual subclass there, so
/// ``isinstance(ApiKeyCredential(k), Credential)`` is True.
///
/// Example::
///
///     from boxlite import ApiKeyCredential
///     cred = ApiKeyCredential("blk_live_...")
///     cred = ApiKeyCredential.from_env()   # reads BOXLITE_API_KEY
#[pyclass(name = "ApiKeyCredential")]
#[derive(Clone)]
pub(crate) struct PyApiKeyCredential {
    key: String,
}

#[pymethods]
impl PyApiKeyCredential {
    #[new]
    fn new(key: String) -> Self {
        Self { key }
    }

    /// Build from `BOXLITE_API_KEY`. Returns `None` when unset/empty.
    #[staticmethod]
    fn from_env() -> Option<Self> {
        std::env::var("BOXLITE_API_KEY")
            .ok()
            .filter(|k| !k.is_empty())
            .map(Self::new)
    }

    /// Return the bearer token. API keys never expire (`expires_at` is
    /// `None`); the SDK core fetches once and caches.
    fn get_token(&self) -> PyAccessToken {
        PyAccessToken {
            token: self.key.clone(),
            expires_at: None,
        }
    }

    fn __repr__(&self) -> &'static str {
        "ApiKeyCredential(***)"
    }
}

/// Configuration for connecting to a remote BoxLite REST API server.
///
/// Example::
///
///     from boxlite import BoxliteRestOptions, ApiKeyCredential
///     opts = BoxliteRestOptions(url="https://api.example.com")
///     opts = BoxliteRestOptions(
///         url="https://api.example.com",
///         credential=ApiKeyCredential("opaque-dashboard-key"),
///     )
///     opts = BoxliteRestOptions.from_env()
///
#[pyclass(name = "BoxliteRestOptions")]
#[derive(Clone)]
pub(crate) struct PyBoxliteRestOptions {
    #[pyo3(get, set)]
    pub(crate) url: String,
    #[pyo3(get, set)]
    pub(crate) credential: Option<PyApiKeyCredential>,
    /// Routing-slot value substituted into the `{prefix}` URL
    /// segment. `None` or empty → URL skips the segment entirely —
    /// the single-tenant deployment shape. Opaque to the client,
    /// deployment decides what it means.
    #[pyo3(get, set)]
    pub(crate) path_prefix: Option<String>,
}

#[pymethods]
impl PyBoxliteRestOptions {
    #[new]
    #[pyo3(signature = (url, credential=None, path_prefix=None))]
    fn new(
        url: String,
        credential: Option<PyApiKeyCredential>,
        path_prefix: Option<String>,
    ) -> Self {
        Self {
            url,
            credential,
            path_prefix,
        }
    }

    /// Create BoxliteRestOptions from environment variables.
    ///
    /// Reads: BOXLITE_REST_URL (required), BOXLITE_API_KEY,
    /// BOXLITE_REST_PATH_PREFIX.
    #[staticmethod]
    fn from_env() -> PyResult<Self> {
        let url = std::env::var("BOXLITE_REST_URL").map_err(|_| {
            crate::util::map_err(boxlite::BoxliteError::Config(
                "BOXLITE_REST_URL not set".into(),
            ))
        })?;
        let path_prefix = std::env::var("BOXLITE_REST_PATH_PREFIX").ok();
        Ok(Self {
            url,
            credential: PyApiKeyCredential::from_env(),
            path_prefix,
        })
    }

    fn __repr__(&self) -> String {
        format!(
            "BoxliteRestOptions(url={:?}, credential={}, path_prefix={:?})",
            self.url,
            if self.credential.is_some() {
                "ApiKeyCredential(***)"
            } else {
                "None"
            },
            self.path_prefix,
        )
    }
}

impl From<PyBoxliteRestOptions> for BoxliteRestOptions {
    fn from(py_opts: PyBoxliteRestOptions) -> Self {
        let mut opts = BoxliteRestOptions::new(py_opts.url);
        if let Some(cred) = py_opts.credential {
            opts = opts.with_api_key(cred.key);
        }
        if let Some(path_prefix) = py_opts.path_prefix {
            opts = opts.with_path_prefix(path_prefix);
        }
        opts
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::advanced_options::{PyContainerCapabilities, PySecurityOptions};

    /// Builds a `PyBoxOptions` with everything at its "untouched" default
    /// except `advanced`, so the conversion under test is exercised without
    /// needing the Python interpreter (plain struct literals, no
    /// `Python::attach` — `cargo test -p boxlite-python` can't link libpython
    /// in this sandbox, but a pure `TryFrom` call between plain Rust structs
    /// doesn't need it).
    fn py_box_options_with_advanced(advanced: PyAdvancedBoxOptions) -> PyBoxOptions {
        PyBoxOptions {
            image: None,
            rootfs_path: None,
            cpus: None,
            memory_mib: None,
            disk_size_gb: None,
            working_dir: None,
            env: vec![],
            volumes: vec![],
            network: None,
            ports: vec![],
            auto_remove: None,
            auto_stop: None,
            auto_delete: None,
            auto_resume: None,
            detach: None,
            entrypoint: None,
            cmd: None,
            user: None,
            tty: None,
            advanced: Some(advanced),
            secrets: vec![],
        }
    }

    /// Same "untouched defaults" shape as above, parameterised on `tty` — the
    /// field the reference server forwards into `boxlite.BoxOptions(**kwargs)`.
    fn py_box_options_with_tty(tty: Option<bool>) -> PyBoxOptions {
        PyBoxOptions {
            image: None,
            rootfs_path: None,
            cpus: None,
            memory_mib: None,
            disk_size_gb: None,
            working_dir: None,
            env: vec![],
            volumes: vec![],
            network: None,
            ports: vec![],
            auto_remove: None,
            auto_stop: None,
            auto_delete: None,
            auto_resume: None,
            detach: None,
            entrypoint: None,
            cmd: None,
            user: None,
            tty,
            advanced: None,
            secrets: vec![],
        }
    }

    fn default_py_security() -> PySecurityOptions {
        PySecurityOptions {
            jailer_enabled: false,
            seccomp_enabled: false,
            max_open_files: None,
            max_file_size: None,
            max_processes: None,
            max_memory: None,
            max_cpu_time: None,
            network_enabled: true,
            close_fds: true,
        }
    }

    /// A caller who only sets `security=` (never mentions `capabilities=`)
    /// must not have `capabilities` silently become an explicit, empty
    /// policy — that's the exact None-vs-Some(empty) distinction this PR's
    /// core API is built to preserve (see AdvancedBoxOptions::capabilities'
    /// own doc comment), and Some(empty) trips `archive_version_for_options`
    /// and `RestRuntime::create`'s `require_linux_capabilities_enabled` gate
    /// for a caller who never touched capabilities at all.
    #[test]
    fn security_only_advanced_options_leaves_capabilities_unspecified() {
        let advanced = PyAdvancedBoxOptions {
            security: Some(default_py_security()),
            health_check: None,
            capabilities: None,
        };

        let opts = BoxOptions::try_from(py_box_options_with_advanced(advanced))
            .expect("security-only options should convert");

        assert!(
            opts.advanced.capabilities().is_none(),
            "expected capabilities to stay unspecified, got {:?}",
            opts.advanced.capabilities()
        );
    }

    /// Mirror of the above for the case a Python caller DOES explicitly ask
    /// for a capability policy — must still come through as `Some`.
    #[test]
    fn explicit_capabilities_still_convert() {
        let advanced = PyAdvancedBoxOptions {
            security: None,
            health_check: None,
            capabilities: Some(PyContainerCapabilities {
                add: vec!["SYS_ADMIN".to_string()],
                drop: vec![],
            }),
        };

        let opts = BoxOptions::try_from(py_box_options_with_advanced(advanced))
            .expect("explicit capabilities should convert");

        let capabilities = opts
            .advanced
            .capabilities()
            .expect("capabilities should be set");
        assert_eq!(capabilities.add, ["SYS_ADMIN"]);
    }

    /// A managed volume is taken verbatim — by id or by name alike. The server
    /// resolves either, so the SDK must not narrow it or rewrite it.
    #[test]
    fn py_managed_volume_is_taken_verbatim() {
        Python::attach(|py| {
            for reference in ["vol_01K2EXAMPLE", "my-data"] {
                let dict = PyDict::new(py);
                dict.set_item("managed_volume", reference).unwrap();
                dict.set_item("guest_path", "/data").unwrap();
                dict.set_item("read_only", true).unwrap();

                let spec = VolumeSpec::from(dict.extract::<PyVolumeSpec>().unwrap());
                assert_eq!(spec.managed_volume.as_deref(), Some(reference));
                assert_eq!(spec.host_path, "");
                assert_eq!(spec.guest_path, "/data");
                assert!(spec.read_only);
            }
        });
    }

    /// A disabled inbound policy maps to `NetworkSpec::Disabled`; an unset
    /// outbound falls back to the enabled default.
    #[test]
    fn nested_network_spec_converts() {
        let (outbound, inbound) = <(NetworkSpec, NetworkSpec)>::try_from(PyNetworkSpec {
            outbound: None,
            inbound: Some(PyInboundNetworkSpec {
                mode: "disabled".into(),
                allow_net: vec![],
            }),
        })
        .unwrap();

        assert!(matches!(inbound, NetworkSpec::Disabled));
        assert!(matches!(outbound, NetworkSpec::Enabled { .. }));
    }

    /// A dropped alias must fail loudly. `ro` was accepted before the rename;
    /// ignoring it as an unknown key would silently turn a read-only mount
    /// into a read-write one — a permission downgrade the caller never sees.
    #[test]
    fn py_volume_dict_rejects_unknown_keys() {
        Python::attach(|py| {
            for (key, value) in [("ro", "true"), ("guest", "/d"), ("source", "my-data")] {
                let dict = PyDict::new(py);
                dict.set_item("host_path", "/tmp/data").unwrap();
                dict.set_item("guest_path", "/data").unwrap();
                dict.set_item(key, value).unwrap();

                let err = dict.extract::<PyVolumeSpec>().unwrap_err();
                let message = err.to_string();
                assert!(message.contains("unknown volume dict key"), "{message}");
                assert!(message.contains(key), "{message}");
            }
        });
    }

    #[test]
    fn py_volume_dict_requires_exactly_one_origin() {
        Python::attach(|py| {
            let both = PyDict::new(py);
            both.set_item("managed_volume", "my-data").unwrap();
            both.set_item("host_path", "/tmp/data").unwrap();
            both.set_item("guest_path", "/data").unwrap();

            let err = both.extract::<PyVolumeSpec>().unwrap_err();
            assert!(err.to_string().contains("not both"), "{err}");

            let neither = PyDict::new(py);
            neither.set_item("guest_path", "/data").unwrap();

            let err = neither.extract::<PyVolumeSpec>().unwrap_err();
            assert!(
                err.to_string().contains("managed_volume or host_path"),
                "{err}"
            );
        });
    }

    /// Tuples stay host binds. They predate managed volumes and have no slot
    /// to say which origin they mean, so a first element that merely looks
    /// like a volume name must not be promoted to a managed volume.
    #[test]
    fn py_volume_tuple_stays_a_host_bind() {
        Python::attach(|py| {
            let tuple = PyTuple::new(py, ["/tmp/data", "/data"]).unwrap();
            let spec = VolumeSpec::from(tuple.extract::<PyVolumeSpec>().unwrap());

            assert_eq!(spec.managed_volume, None);
            assert_eq!(spec.host_path, "/tmp/data");
            assert_eq!(spec.guest_path, "/data");
        });
    }

    /// `tty` is a concrete `bool` on core `BoxOptions`, not an `Option`, so the
    /// conversion has to distinguish "caller asked" from "caller said nothing".
    /// Without the `if let Some(tty)` arm this silently stays false and
    /// `boxlite run -t` against the reference server produces a box with no
    /// terminal — the same class of silent drop this whole change closes.
    #[test]
    fn explicit_tty_reaches_core_box_options() {
        let opts = BoxOptions::try_from(py_box_options_with_tty(Some(true)))
            .expect("tty=True should convert");

        assert!(opts.tty, "expected tty to reach core BoxOptions");
    }

    /// The other side: an unset `tty` must leave the core default alone rather
    /// than writing `false` over whatever the core decides.
    #[test]
    fn unset_tty_preserves_the_core_default() {
        let opts =
            BoxOptions::try_from(py_box_options_with_tty(None)).expect("tty=None should convert");

        assert_eq!(
            opts.tty,
            BoxOptions::default().tty,
            "an unset tty must not overwrite the core default"
        );
    }
}
