//! Krun - VMM implementation using libkrun.

use super::context::KrunContext;
use crate::runtime::constants::network;
use crate::vmm::{InstanceSpec, Vmm, VmmConfig, VmmInstance, engine::VmmInstanceImpl};
use boxlite_shared::errors::{BoxliteError, BoxliteResult};

/// Libkrun-specific VMM instance implementation.
struct KrunVmmInstance {
    context: KrunContext,
}

impl VmmInstanceImpl for KrunVmmInstance {
    fn enter(self: Box<Self>) -> BoxliteResult<()> {
        // Actually start the VM - following microsandbox pattern
        // In libkrun:
        // - Success: krun_start_enter never returns (process becomes VM)
        // - Failure: returns negative error code
        // - Guest exit: returns positive exit status (guest process exit code)
        //
        // IMPORTANT: libkrun takes over the current process completely.
        // This means:
        // 1. Unit tests cannot test this functionality (process takeover incompatible with test harness)
        // 2. This must run in the main application process or a separate process
        // 3. libkrunfw must be available in system library paths for successful VM creation
        let status = unsafe { self.context.start_enter() };

        // If we reach here, either:
        // 1. VM failed to start (negative status)
        // 2. VM started and guest exited (non-negative status) - this is success
        if status < 0 {
            // VM failed to start
            if status == -22 {
                return Err(BoxliteError::Engine("libkrun returned EINVAL.".into()));
            }
            Err(BoxliteError::Engine(format!(
                "VM failed to start with status {status}"
            )))
        } else {
            // VM started and guest exited successfully (status is guest exit code)
            Ok(())
        }
    }
}

/// Krun handles VM execution using the libkrun hypervisor.
///
/// This engine is responsible for creating Box instances with the provided
/// configuration. The actual VM execution happens when `VmmInstance::enter()`
/// is called, which performs process takeover via libkrun's `krun_start_enter()`.
pub struct Krun {
    #[allow(dead_code)]
    options: VmmConfig,
}

impl Krun {
    /// Create a new Krun engine with the specified options.
    ///
    /// # Arguments
    /// * `options` - Engine configuration options
    ///
    /// # Returns
    /// * `Ok(Krun)` - Successfully created engine
    /// * `Err(...)` - Failed to detect libkrun library
    pub fn new(options: VmmConfig) -> BoxliteResult<Self> {
        Ok(Self { options })
    }

    /// Transform Unix socket URIs to vsock URIs in a shell command string.
    ///
    /// Replaces `--{arg_name} unix://...` with `--{arg_name} vsock://PORT`
    fn transform_shell_arg_unix_to_vsock(input: &str, arg_name: &str, vsock_port: u32) -> String {
        use boxlite_shared::Transport;
        let vsock_uri = Transport::vsock(vsock_port).to_uri();
        let pattern = format!("--{} unix://", arg_name);

        let mut result = String::new();
        let mut chars = input.chars().peekable();
        let mut pos = 0;

        while let Some(c) = chars.next() {
            // Check if we're at the start of the pattern
            if c == '-' && input[pos..].starts_with(&pattern) {
                // Copy "--{arg_name} "
                result.push_str(&format!("--{} ", arg_name));

                // Skip past the pattern
                let skip_len = pattern.len() - 1; // -1 because we already consumed '-'
                for _ in 0..skip_len {
                    chars.next();
                }
                pos += pattern.len();

                // Skip the rest of the unix path until whitespace
                while let Some(&next) = chars.peek() {
                    if next.is_whitespace() {
                        break;
                    }
                    chars.next();
                    pos += 1;
                }

                // Add the vsock URI
                result.push_str(&vsock_uri);
            } else {
                result.push(c);
                pos += c.len_utf8();
            }
        }

        result
    }

    /// Transform a single Unix socket argument to vsock.
    ///
    /// Handles two cases:
    /// 1. Separate arguments: ["--{arg_name}", "unix://..."]
    /// 2. Shell command string: ["-c", "... --{arg_name} unix://... "]
    fn transform_arg_unix_to_vsock(guest_args: &mut [String], arg_name: &str, vsock_port: u32) {
        use boxlite_shared::Transport;
        let vsock_uri = Transport::vsock(vsock_port).to_uri();
        let pattern = format!("--{} unix://", arg_name);

        for i in 0..guest_args.len() {
            // Case 1: Separate arguments ["--{arg_name}", "unix://..."]
            if guest_args[i] == format!("--{}", arg_name)
                && i + 1 < guest_args.len()
                && guest_args[i + 1].starts_with("unix://")
            {
                tracing::debug!(
                    arg = arg_name,
                    original = %guest_args[i + 1],
                    transformed = %vsock_uri,
                    "Transforming Unix socket to vsock URI"
                );
                guest_args[i + 1] = vsock_uri;
                return;
            }

            // Case 2: Shell command string (e.g., -c "... --{arg_name} unix://... ")
            if guest_args[i].contains(&pattern) {
                let transformed =
                    Self::transform_shell_arg_unix_to_vsock(&guest_args[i], arg_name, vsock_port);
                tracing::debug!(
                    arg = arg_name,
                    original = %guest_args[i],
                    transformed = %transformed,
                    "Transforming shell command string"
                );
                guest_args[i] = transformed;
                return;
            }
        }
    }

    /// Transform guest arguments to replace Unix socket URIs with vsock URIs.
    ///
    /// Transforms both --listen and --notify from Unix to vsock.
    /// The engine bridges Unix sockets on host to vsock ports inside VM.
    fn transform_guest_args(mut guest_args: Vec<String>) -> Vec<String> {
        // Transform --listen unix://... -> --listen vsock://2695
        Self::transform_arg_unix_to_vsock(&mut guest_args, "listen", network::GUEST_AGENT_PORT);

        // Transform --notify unix://... -> --notify vsock://2696
        Self::transform_arg_unix_to_vsock(&mut guest_args, "notify", network::GUEST_READY_PORT);

        guest_args
    }

    fn set_entrypoint(config: &InstanceSpec, ctx: &mut KrunContext) -> Result<(), BoxliteError> {
        // Prepare entrypoint - the VM runs the guest agent which will:
        // 1. Mount virtiofs shares
        // 2. Create overlayfs from layers
        // 3. Execute the container entrypoint inside the mounted filesystem
        let guest_executable = &config.guest_entrypoint.executable;

        // Transform guest arguments (engine handles transport-specific transformations)
        let guest_args = Self::transform_guest_args(config.guest_entrypoint.args.clone());
        tracing::debug!(executable = %guest_executable,
                            args_count = guest_args.len(),
                            "Configuring entrypoint");
        for (i, arg) in guest_args.iter().enumerate() {
            tracing::trace!(index = i, arg = ?arg, "Entrypoint argument");
        }

        // Set executable and arguments with provided environment
        unsafe {
            ctx.set_exec(guest_executable, &guest_args, &config.guest_entrypoint.env)?;
        }
        Ok(())
    }
}

impl Vmm for Krun {
    fn create(&mut self, config: InstanceSpec) -> BoxliteResult<VmmInstance> {
        tracing::trace!("Step into Krun::create");

        // Validate filesystem shares exist
        for share in config.fs_shares.shares() {
            if !share.host_path.exists() {
                return Err(BoxliteError::Engine(format!(
                    "Filesystem share directory '{}' not found: {}",
                    share.tag,
                    share.host_path.display()
                )));
            }
            tracing::debug!(
                tag = %share.tag,
                path = %share.host_path.display(),
                read_only = share.read_only,
                "Validated filesystem share"
            );
        }

        // Validate disk images exist
        for block_device in config.block_devices.devices() {
            if !block_device.disk_path.exists() {
                return Err(BoxliteError::Engine(format!(
                    "Disk image not found: {}",
                    block_device.disk_path.display()
                )));
            }
            tracing::debug!(
                block_id = %block_device.block_id,
                path = %block_device.disk_path.display(),
                format = %block_device.format.as_str(),
                read_only = block_device.read_only,
                "Validated disk image"
            );
        }

        // Create and configure libkrun context
        let ctx = unsafe {
            tracing::debug!("Initializing libkrun logging system");
            if let Err(e) = KrunContext::init_logging() {
                tracing::warn!("Failed to initialize libkrun logging: {}", e);
            }

            tracing::debug!("Creating libkrun context");
            let mut ctx = KrunContext::create()?;

            tracing::debug!("Setting VM config: 4 CPUs, 4096MB memory");
            // Configure VM like chroot_vm example: 4 CPUs and 4096MB memory
            ctx.set_vm_config(config.cpus.unwrap_or(4), config.memory_mib.unwrap_or(4096))?;

            // Configure net from connection info passed by parent process
            if let Some(connection) = &config.network_backend_endpoint {
                tracing::info!(connection = ?connection, "Configuring network connection");

                match connection {
                    crate::net::NetworkBackendEndpoint::UnixSocket {
                        path,
                        connection_type,
                        mac_address,
                    } => {
                        // Pass the Unix socket path to libkrun
                        // IMPORTANT: We pass the PATH, not a connected FD, so that libkrun can:
                        //  1. Connect to the socket itself
                        //  2. Send the VFKit magic handshake at the right time (when NET_FLAG_VFKIT is set)
                        tracing::info!(
                            path = ?path,
                            connection_type = ?connection_type,
                            mac_address = ?mac_address,
                            "Configuring Unix socket net"
                        );

                        // Convert path to string for FFI
                        let socket_path_str = path.to_str().ok_or_else(|| {
                            BoxliteError::Network(format!(
                                "Socket path contains invalid UTF-8: {}",
                                path.display()
                            ))
                        })?;

                        // Configure virtio-net feature flags
                        use crate::vmm::krun::constants::network_features::*;
                        let features = NET_FEATURE_CSUM
                            | NET_FEATURE_GUEST_CSUM
                            | NET_FEATURE_GUEST_TSO4
                            | NET_FEATURE_GUEST_UFO
                            | NET_FEATURE_HOST_TSO4
                            | NET_FEATURE_HOST_UFO;

                        // Pass the socket path to libkrun (not FD)
                        // libkrun will connect and send the VFKit magic handshake if needed
                        ctx.add_net_path(
                            socket_path_str,
                            features,
                            *connection_type,
                            *mac_address,
                        )?;

                        tracing::debug!("Successfully configured Unix socket net");
                    }
                }
            } else {
                // No network connection specified - use libkrun's built-in TSI net
                tracing::debug!("No network backend - using libkrun's built-in TSI net");
            }

            // Raise RLIMIT_NOFILE to maximum - CRITICAL for virtio-fs!
            // This must be done BEFORE mounting virtiofs shares
            tracing::debug!("Raising RLIMIT_NOFILE for virtio-fs");
            #[cfg(unix)]
            {
                use libc::{RLIMIT_NOFILE, getrlimit, rlimit, setrlimit};
                let mut rlim = rlimit {
                    rlim_cur: 0,
                    rlim_max: 0,
                };
                if getrlimit(RLIMIT_NOFILE, &mut rlim) == 0 {
                    rlim.rlim_cur = rlim.rlim_max;
                    if setrlimit(RLIMIT_NOFILE, &rlim) != 0 {
                        tracing::warn!("Failed to raise RLIMIT_NOFILE");
                    } else {
                        tracing::debug!(limit = rlim.rlim_cur, "RLIMIT_NOFILE raised");
                    }
                } else {
                    tracing::warn!("Failed to get RLIMIT_NOFILE");
                }
            }

            // Configure rlimits that will be set in the guest
            // Format: "RLIMIT_NAME=soft:hard" where soft and hard are limits
            // These limits ensure the guest has adequate resources for container workloads
            let rlimits = vec![
                "6=4096:8192".to_string(),       // RLIMIT_NPROC = 6
                "7=1048576:1048576".to_string(), // RLIMIT_NOFILE = 7
            ];
            tracing::debug!("Configuring guest rlimits: {:?}", rlimits);
            ctx.set_rlimits(&rlimits)?;

            // Add filesystem shares via virtiofs
            tracing::info!("Adding filesystem shares via virtiofs:");
            for share in config.fs_shares.shares() {
                let path_str = share.host_path.to_str().ok_or_else(|| {
                    BoxliteError::Engine(format!("Invalid path: {}", share.host_path.display()))
                })?;

                tracing::info!(
                    "  {} → {} ({})",
                    share.tag,
                    share.host_path.display(),
                    if share.read_only { "ro" } else { "rw" }
                );
                ctx.add_virtiofs(&share.tag, path_str)?;
            }

            // Attach disk images via virtio-blk
            if !config.block_devices.devices().is_empty() {
                tracing::info!("Attaching block devices:");
                for disk in config.block_devices.devices() {
                    let path_str = disk.disk_path.to_str().ok_or_else(|| {
                        BoxliteError::Engine(format!(
                            "Invalid disk path: {}",
                            disk.disk_path.display()
                        ))
                    })?;

                    tracing::info!(
                        "  {} → {} ({}, {})",
                        disk.block_id,
                        disk.disk_path.display(),
                        disk.format.as_str(),
                        if disk.read_only {
                            "read-only"
                        } else {
                            "read-write"
                        }
                    );

                    ctx.add_disk_with_format(
                        &disk.block_id,
                        path_str,
                        disk.read_only,
                        disk.format.as_str(),
                    )?;
                }
            }

            // Configure root filesystem based on init rootfs strategy
            if let crate::runtime::initrf::Strategy::Disk {
                device_path: Some(device_path),
                ..
            } = &config.init_rootfs.strategy
            {
                // Disk-based boot: use set_root_disk_remount
                tracing::info!("Configuring init disk remount: {}", device_path);
                ctx.set_root_disk_remount(device_path, Some("ext4"), None)?;
            } else {
                // Virtiofs-based boot: use set_rootfs
                let rootfs_str = config.init_rootfs.path.to_str().ok_or_else(|| {
                    BoxliteError::Engine(format!(
                        "Invalid rootfs path: {}",
                        config.init_rootfs.path.display()
                    ))
                })?;
                tracing::debug!("Setting VM root filesystem (virtiofs): {}", rootfs_str);
                ctx.set_rootfs(rootfs_str)?;
            }

            tracing::debug!("Setting working directory to /");
            // Set working directory (default to root if not specified)
            ctx.set_workdir("/boxlite")?;

            Self::set_entrypoint(&config, &mut ctx)?;

            // Configure gRPC communication channel (Unix socket bridged to vsock)
            // listen=true: libkrun creates socket, host connects, guest accepts via vsock
            let grpc_socket_path = match &config.transport {
                boxlite_shared::Transport::Unix { socket_path } => socket_path
                    .to_str()
                    .ok_or_else(|| BoxliteError::Engine("invalid gRPC socket path".into()))?,
                _ => {
                    return Err(BoxliteError::Engine(
                        "gRPC transport must be Unix socket on host side".into(),
                    ));
                }
            };
            tracing::debug!(
                socket_path = grpc_socket_path,
                guest_port = network::GUEST_AGENT_PORT,
                "Configuring vsock bridge for gRPC"
            );
            ctx.add_vsock_port(network::GUEST_AGENT_PORT, grpc_socket_path, true)?;

            // Configure ready notification channel (Unix socket bridged to vsock)
            // listen=false: host creates socket and listens, guest connects via vsock
            let ready_socket_path = match &config.ready_transport {
                boxlite_shared::Transport::Unix { socket_path } => socket_path
                    .to_str()
                    .ok_or_else(|| BoxliteError::Engine("invalid ready socket path".into()))?,
                _ => {
                    return Err(BoxliteError::Engine(
                        "ready transport must be Unix socket on host side".into(),
                    ));
                }
            };
            tracing::debug!(
                socket_path = ready_socket_path,
                guest_port = network::GUEST_READY_PORT,
                "Configuring vsock bridge for ready notification"
            );
            ctx.add_vsock_port(network::GUEST_READY_PORT, ready_socket_path, false)?;

            // Configure console output redirection if specified
            if let Some(console_path) = &config.console_output {
                let console_path_str = console_path.to_str().ok_or_else(|| {
                    BoxliteError::Engine(format!(
                        "Invalid console output path: {}",
                        console_path.display()
                    ))
                })?;
                tracing::debug!(
                    console_path = console_path_str,
                    "Redirecting console output"
                );
                ctx.set_console_output(console_path_str)?;
            }

            ctx
        };

        // Return a VmmInstance that wraps the configured context
        // The actual VM start will happen when enter() is called
        let instance = KrunVmmInstance { context: ctx };
        Ok(VmmInstance::new(Box::new(instance)))
    }
}
