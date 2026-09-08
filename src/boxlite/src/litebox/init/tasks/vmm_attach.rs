//! Task: VMM Attach - Attach to an existing running VM process.
//!
//! Creates a handler for an already-running VM subprocess by PID. Used
//! for reconnecting to detached boxes.
//!
//! Identity is read from the canonical PID file (`shim.pid`) and verified
//! via start-time fingerprint. `state.pid` from the DB is a cache that
//! could lag (PID reuse, external kill); the file + ProcessIdentity is
//! the trust anchor.
//!
//! Crash enrichment: when ProcessIdentity is Absent AND the shim left a
//! parseable exit file behind, the error carries the formatted
//! CrashReport so callers see the actual crash cause instead of a
//! generic "shim no longer alive" message.

use super::{InitCtx, task_start};
use crate::litebox::CrashReport;
use crate::net::NetworkBackendConfig;
use crate::pipeline::PipelineTask;
use crate::runtime::options::NetworkSpec;
use crate::runtime::rt_impl::stash_exit_file;
use crate::util::{PidFileReader, ProcessIdentity};
use crate::vmm::ExitInfo;
use crate::vmm::controller::ShimHandler;
use async_trait::async_trait;
use boxlite_shared::errors::{BoxliteError, BoxliteResult};

pub struct VmmAttachTask;

#[async_trait]
impl PipelineTask<InitCtx> for VmmAttachTask {
    async fn run(self: Box<Self>, ctx: InitCtx) -> BoxliteResult<()> {
        let task_name = self.name();
        let box_id = task_start(&ctx, task_name).await;

        let (runtime, config_id, network) = {
            let ctx = ctx.lock().await;
            // Reattach still owns a control backend for the box's live gvproxy.
            let network = match &ctx.config.options.network {
                NetworkSpec::Enabled { allow_net } => Some((
                    allow_net.clone(),
                    ctx.config.options.secrets.clone(),
                    ctx.config.options.net_bandwidth,
                )),
                NetworkSpec::Disabled => None,
            };
            (ctx.runtime.clone(), ctx.config.id.clone(), network)
        };

        let layout = runtime.layout.box_layout(config_id.as_str(), false)?;
        let pid_file = layout.pid_file_path();
        let exit_file = layout.exit_file_path();

        let pid = match PidFileReader::at(&pid_file).process_identity() {
            ProcessIdentity::Verified(pid) | ProcessIdentity::Legacy(pid) => {
                // Live shim wins — archive any prior-lifecycle exit file
                // so a future crash gets the canonical slot.
                if exit_file.exists() {
                    stash_exit_file(&layout);
                    tracing::warn!(
                        box_id = %box_id,
                        pid,
                        "Live shim found alongside stale exit file; stashed to exit.previous"
                    );
                }
                pid
            }
            ProcessIdentity::Absent => {
                // No live shim. If an exit file is present, surface the
                // crash cause; otherwise fail with a generic message.
                let msg = if ExitInfo::from_file(&exit_file).is_some() {
                    let report = CrashReport::from_exit_file(
                        &exit_file,
                        &layout.console_output_path(),
                        &layout.stderr_file_path(),
                        box_id.as_str(),
                        None,
                    );
                    // This path logged nothing before: the crash cause reached
                    // the caller and nowhere else. Route the detail to the host
                    // log so it survives the terse caller-facing message.
                    tracing::error!(
                        box_id = %box_id,
                        "Box crashed in a prior lifecycle:\n{}",
                        report.operator_report
                    );
                    report.client_message
                } else {
                    "Box process is no longer running (PID file missing, process dead, \
                     or start-time mismatch indicating PID reuse)"
                        .to_string()
                };
                return Err(BoxliteError::InvalidState(msg));
            }
        };

        let handler = ShimHandler::from_pid(pid, config_id);

        // The box's one network backend on the reattach path: control over the
        // live gvproxy. No wire spec is produced on reattach; PortPublishTask
        // uses this client to adopt or repair forwards before LiveState is
        // returned.
        let network_backend = network.and_then(|(allow_net, secrets, net_bandwidth)| {
            let config = NetworkBackendConfig {
                socket_path: layout.net_backend_socket_path(),
                allow_net,
                secrets,
                ca_dir: layout.ca_dir(),
                net_bandwidth,
            };
            runtime.network_factory.create(&config)
        });

        let mut ctx = ctx.lock().await;
        ctx.guard.set_layout(layout.clone());
        ctx.layout = Some(layout);
        ctx.guard.set_handler(Box::new(handler));
        ctx.network_backend = network_backend;

        tracing::info!(
            box_id = %box_id,
            pid,
            "Attached to existing VM process"
        );

        Ok(())
    }

    fn name(&self) -> &str {
        "vmm_attach"
    }
}
