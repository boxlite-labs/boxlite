#![allow(unsafe_op_in_unsafe_fn, non_local_definitions)]

mod advanced_options;
mod box_handle;
mod copy_stream;
mod exec;
mod images;
mod info;
mod metrics;
mod network;
mod options;
mod runtime;
mod snapshot_options;
mod snapshots;
mod util;
mod volumes;

use crate::advanced_options::{
    PyAdvancedBoxOptions, PyContainerCapabilities, PyHealthCheckOptions, PySecurityOptions,
};
use crate::box_handle::PyBox;
use crate::exec::{PyExecStderr, PyExecStdin, PyExecStdout, PyExecution};
use crate::images::{PyImageHandle, PyImageInfo, PyImagePullResult};
use crate::info::{
    PyBoxInfo, PyBoxStateInfo, PyHealthState, PyHealthStatus, PyInboundNetworkInfo, PyNetworkInfo,
    PyOutboundNetworkInfo, PyPublishedPort,
};
use crate::metrics::{PyBoxMetrics, PyRuntimeMetrics};
use crate::network::{
    PyBoxConnection, PyBoxTunnel, PyNetworkHandle, PySocketAddress, PyTunnelForwarder,
};
use crate::options::{
    PyAccessToken, PyApiKeyCredential, PyBoxOptions, PyBoxliteRestOptions, PyCopyOptions,
    PyImageRegistry, PyInboundNetworkSpec, PyNetworkSpec, PyOptions, PyOutboundNetworkSpec,
    PySecret,
};
use crate::runtime::PyBoxlite;
use crate::snapshot_options::{PyCloneOptions, PyExportOptions, PySnapshotOptions};
use crate::snapshots::{PySnapshotHandle, PySnapshotInfo};
use crate::volumes::{PyVolumeHandle, PyVolumeInfo};
use pyo3::prelude::*;

#[pymodule(name = "boxlite")]
fn boxlite_python(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyOptions>()?;
    m.add_class::<PyImageRegistry>()?;
    m.add_class::<PyBoxOptions>()?;
    m.add_class::<PyNetworkSpec>()?;
    m.add_class::<PyOutboundNetworkSpec>()?;
    m.add_class::<PyInboundNetworkSpec>()?;
    m.add_class::<PySecurityOptions>()?;
    m.add_class::<PyHealthCheckOptions>()?;
    m.add_class::<PyAdvancedBoxOptions>()?;
    m.add_class::<PyContainerCapabilities>()?;
    m.add_class::<PyBoxlite>()?;
    m.add_class::<PyBox>()?;
    m.add_class::<PyExecution>()?;
    m.add_class::<PyExecStdin>()?;
    m.add_class::<PyExecStdout>()?;
    m.add_class::<PyExecStderr>()?;
    m.add_class::<crate::copy_stream::PyCopyInStream>()?;
    m.add_class::<crate::copy_stream::PyCopyOutStream>()?;
    m.add_class::<PyImageHandle>()?;
    m.add_class::<PyImageInfo>()?;
    m.add_class::<PyImagePullResult>()?;
    m.add_class::<PyVolumeHandle>()?;
    m.add_class::<PyVolumeInfo>()?;
    m.add_class::<PyBoxInfo>()?;
    m.add_class::<PyBoxStateInfo>()?;
    m.add_class::<PyNetworkInfo>()?;
    m.add_class::<PyOutboundNetworkInfo>()?;
    m.add_class::<PyInboundNetworkInfo>()?;
    m.add_class::<PyPublishedPort>()?;
    m.add_class::<PyHealthState>()?;
    m.add_class::<PyHealthStatus>()?;
    m.add_class::<PyRuntimeMetrics>()?;
    m.add_class::<PyBoxMetrics>()?;
    m.add_class::<PyNetworkHandle>()?;
    m.add_class::<PyBoxTunnel>()?;
    m.add_class::<PyBoxConnection>()?;
    m.add_class::<PySocketAddress>()?;
    m.add_class::<PyTunnelForwarder>()?;
    m.add_class::<PyCopyOptions>()?;
    m.add_class::<PySnapshotInfo>()?;
    m.add_class::<PySnapshotHandle>()?;
    m.add_class::<PySnapshotOptions>()?;
    m.add_class::<PyExportOptions>()?;
    m.add_class::<PyCloneOptions>()?;
    m.add_class::<PyHealthCheckOptions>()?;
    m.add_class::<PyBoxliteRestOptions>()?;
    m.add_class::<PyApiKeyCredential>()?;
    m.add_class::<PyAccessToken>()?;
    m.add_class::<PySecret>()?;

    Ok(())
}
