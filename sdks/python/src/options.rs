use std::path::PathBuf;

use boxlite::runtime::constants::images;
use boxlite::runtime::options::{
    BoxOptions, BoxliteOptions, NetworkSpec, PortProtocol, PortSpec, RootfsSpec, VolumeSpec,
};
use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use pyo3::types::{PyAnyMethods, PyDict, PyTuple};

#[pyclass(name = "Options")]
#[derive(Clone, Debug)]
pub(crate) struct PyOptions {
    #[pyo3(get, set)]
    pub(crate) home_dir: Option<String>,
}

#[pymethods]
impl PyOptions {
    #[new]
    #[pyo3(signature = (home_dir=None))]
    fn new(home_dir: Option<String>) -> Self {
        Self { home_dir }
    }

    fn __repr__(&self) -> String {
        format!("Options(home_dir={:?})", self.home_dir)
    }
}

impl From<PyOptions> for BoxliteOptions {
    fn from(py_opts: PyOptions) -> Self {
        let mut config = BoxliteOptions::default();

        if let Some(home_dir) = py_opts.home_dir {
            config.home_dir = PathBuf::from(home_dir);
        }

        config
    }
}

#[pyclass(name = "BoxOptions")]
#[derive(Clone, Debug)]
pub(crate) struct PyBoxOptions {
    #[pyo3(get, set)]
    pub(crate) image: Option<String>,
    #[pyo3(get, set)]
    pub(crate) rootfs_path: Option<String>,
    #[pyo3(get, set)]
    pub(crate) name: Option<String>,
    #[pyo3(get, set)]
    pub(crate) cpus: Option<u8>,
    #[pyo3(get, set)]
    pub(crate) memory_mib: Option<u32>,
    #[pyo3(get, set)]
    pub(crate) working_dir: Option<String>,
    #[pyo3(get, set)]
    pub(crate) env: Vec<(String, String)>,
    pub(crate) volumes: Vec<PyVolumeSpec>,
    #[pyo3(get, set)]
    pub(crate) network: Option<String>,
    pub(crate) ports: Vec<PyPortSpec>,
}

#[pymethods]
impl PyBoxOptions {
    #[new]
    #[pyo3(signature = (
        image=None,
        rootfs_path=None,
        name=None,
        cpus=None,
        memory_mib=None,
        working_dir=None,
        env=vec![],
        volumes=vec![],
        network=None,
        ports=vec![]
    ))]
    #[allow(clippy::too_many_arguments)]
    fn new(
        image: Option<String>,
        rootfs_path: Option<String>,
        name: Option<String>,
        cpus: Option<u8>,
        memory_mib: Option<u32>,
        working_dir: Option<String>,
        env: Vec<(String, String)>,
        volumes: Vec<PyVolumeSpec>,
        network: Option<String>,
        ports: Vec<PyPortSpec>,
    ) -> Self {
        Self {
            image,
            rootfs_path,
            name,
            cpus,
            memory_mib,
            working_dir,
            env,
            volumes,
            network,
            ports,
        }
    }

    fn __repr__(&self) -> String {
        format!(
            "BoxOptions(image={:?}, rootfs_path={:?}, cpus={:?}, memory_mib={:?})",
            self.image, self.rootfs_path, self.cpus, self.memory_mib
        )
    }
}

impl From<PyBoxOptions> for BoxOptions {
    fn from(py_opts: PyBoxOptions) -> Self {
        let rootfs = match py_opts.rootfs_path {
            Some(path) if !path.is_empty() => RootfsSpec::RootfsPath(path),
            _ => RootfsSpec::Image(py_opts.image.unwrap_or_else(|| images::DEFAULT.to_string())),
        };

        let volumes = py_opts.volumes.into_iter().map(VolumeSpec::from).collect();

        let network = match py_opts.network {
            // Some(ref s) if s.eq_ignore_ascii_case("host") => NetworkSpec::Host,
            Some(ref s) if s.eq_ignore_ascii_case("isolated") => NetworkSpec::Isolated,
            // Some(s) if !s.is_empty() => NetworkSpec::Custom(s),
            _ => NetworkSpec::Isolated,
        };

        let ports = py_opts.ports.into_iter().map(PortSpec::from).collect();

        BoxOptions {
            name: py_opts.name,
            rootfs,
            cpus: py_opts.cpus,
            memory_mib: py_opts.memory_mib,
            working_dir: py_opts.working_dir,
            env: py_opts.env,
            volumes,
            network,
            ports,
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct PyVolumeSpec {
    host: String,
    guest: String,
    read_only: bool,
}

impl From<PyVolumeSpec> for VolumeSpec {
    fn from(v: PyVolumeSpec) -> Self {
        VolumeSpec {
            host_path: v.host,
            guest_path: v.guest,
            read_only: v.read_only,
        }
    }
}

impl<'a, 'py> pyo3::FromPyObject<'a, 'py> for PyVolumeSpec {
    type Error = PyErr;

    fn extract(ob: Borrowed<'a, 'py, PyAny>) -> PyResult<Self> {
        let obj = ob.to_owned();

        if let Ok(t) = obj.cast::<PyTuple>() {
            let len = t.len();
            let err =
                || PyRuntimeError::new_err("volumes tuples must be (host, guest[, read_only])");
            let host: String;
            let guest: String;
            let read_only: bool;

            match len {
                2 => {
                    host = t.get_item(0)?.extract()?;
                    guest = t.get_item(1)?.extract()?;
                    read_only = false;
                }
                3 => {
                    host = t.get_item(0)?.extract()?;
                    guest = t.get_item(1)?.extract()?;
                    read_only = t.get_item(2)?.extract()?;
                }
                _ => return Err(err()),
            }

            return Ok(PyVolumeSpec {
                host,
                guest,
                read_only,
            });
        }

        if let Ok(d) = obj.cast::<PyDict>() {
            let host: String = if let Ok(Some(v)) = d.get_item("host") {
                v.extract()?
            } else if let Ok(Some(v)) = d.get_item("host_path") {
                v.extract()?
            } else {
                return Err(PyRuntimeError::new_err(
                    "volume dict missing host/host_path",
                ));
            };

            let guest: String = if let Ok(Some(v)) = d.get_item("guest") {
                v.extract()?
            } else if let Ok(Some(v)) = d.get_item("guest_path") {
                v.extract()?
            } else {
                return Err(PyRuntimeError::new_err(
                    "volume dict missing guest/guest_path",
                ));
            };

            let read_only: bool = if let Ok(Some(v)) = d.get_item("read_only") {
                v.extract()?
            } else if let Ok(Some(v)) = d.get_item("ro") {
                v.extract()?
            } else {
                false
            };

            return Ok(PyVolumeSpec {
                host,
                guest,
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
