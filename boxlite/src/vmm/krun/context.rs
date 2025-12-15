//! High-level context wrapper for libkrun interactions.
//!
//! All unsafe functions in this module wrap libkrun FFI calls.
//! They are marked unsafe because they call into C code and require
//! the caller to ensure the KrunContext is valid.

#![allow(clippy::missing_safety_doc)]

use std::{ffi::CString, ptr};

use crate::vmm::krun::check_status;
use boxlite_shared::errors::{BoxliteError, BoxliteResult};
use libkrun_sys::{
    krun_add_disk2, krun_add_net_unixgram, krun_add_net_unixstream, krun_add_virtiofs,
    krun_add_vsock_port2, krun_create_ctx, krun_free_ctx, krun_init_log, krun_set_console_output,
    krun_set_env, krun_set_exec, krun_set_gpu_options, krun_set_kernel, krun_set_nested_virt,
    krun_set_port_map, krun_set_rlimits, krun_set_root, krun_set_root_disk_remount,
    krun_set_vm_config, krun_set_workdir, krun_setgid, krun_setuid, krun_split_irqchip,
    krun_start_enter,
};

/// Thin wrapper that owns a libkrun context.
pub struct KrunContext {
    ctx_id: u32,
}

impl KrunContext {
    #[allow(dead_code)]
    pub fn id(&self) -> u32 {
        self.ctx_id
    }

    /// Initialize libkrun logging system based on RUST_LOG environment variable.
    /// Must be called before creating any context.
    pub unsafe fn init_logging() -> BoxliteResult<()> {
        use libkrun_sys::{
            KRUN_LOG_LEVEL_DEBUG, KRUN_LOG_LEVEL_ERROR, KRUN_LOG_LEVEL_INFO, KRUN_LOG_LEVEL_TRACE,
            KRUN_LOG_STYLE_AUTO, KRUN_LOG_TARGET_STDERR,
        };

        // Determine log level from RUST_LOG environment variable
        let log_level = match std::env::var("RUST_LOG").as_deref() {
            Ok("trace") | Ok("boxlite=trace") => {
                tracing::debug!("Initializing libkrun with TRACE log level");
                KRUN_LOG_LEVEL_TRACE
            }
            Ok("debug") | Ok("boxlite=debug") => {
                tracing::debug!("Initializing libkrun with DEBUG log level");
                KRUN_LOG_LEVEL_DEBUG
            }
            Ok("info") | Ok("boxlite=info") => {
                tracing::debug!("Initializing libkrun with INFO log level");
                KRUN_LOG_LEVEL_INFO
            }
            _ => KRUN_LOG_LEVEL_ERROR, // Default: only show errors
        };

        let log_target = KRUN_LOG_TARGET_STDERR; // Output to stderr so it's captured
        let log_style = KRUN_LOG_STYLE_AUTO; // Auto-detect color support
        let flags = 0;
        tracing::trace!(
            "Calling krun_init_log({:?}) with log_target: {}, log_level: {}, log_style: {}, flags: {}",
            krun_init_log as *const (),
            log_target,
            log_level,
            log_style,
            flags
        );
        check_status("krun_init_log", unsafe {
            krun_init_log(log_target, log_level, log_style, flags)
        })
    }

    #[allow(unsafe_op_in_unsafe_fn)]
    pub unsafe fn create() -> BoxliteResult<Self> {
        tracing::trace!("Calling krun_create_ctx()");
        let ctx = unsafe { krun_create_ctx() };
        if ctx < 0 {
            tracing::error!(status = ctx, "krun_create_ctx failed");
            return Err(BoxliteError::Engine(format!(
                "krun_create_ctx failed with status {ctx}"
            )));
        }
        tracing::trace!(ctx_id = ctx, "krun_create_ctx succeeded");
        Ok(Self { ctx_id: ctx as u32 })
    }

    pub unsafe fn set_vm_config(&self, cpus: u8, memory_mib: u32) -> BoxliteResult<()> {
        check_status("krun_set_vm_config", unsafe {
            krun_set_vm_config(self.ctx_id, cpus, memory_mib)
        })
    }

    pub unsafe fn set_rootfs(&self, rootfs: &str) -> BoxliteResult<()> {
        tracing::trace!("Setting rootfs to: {}", rootfs);
        tracing::trace!(
            "Checking if rootfs exists: {}",
            std::path::Path::new(rootfs).exists()
        );
        let rootfs_c = CString::new(rootfs)
            .map_err(|e| BoxliteError::Engine(format!("invalid rootfs path: {e}")))?;
        check_status("krun_set_root", unsafe {
            krun_set_root(self.ctx_id, rootfs_c.as_ptr())
        })
    }

    /// Configure root filesystem backed by a block device with automatic remount.
    ///
    /// This allows booting from a disk image. Libkrun creates a dummy virtiofs root,
    /// executes init from it, and then automatically pivots to the disk-based root.
    ///
    /// # Arguments
    /// * `device` - Block device path (e.g., "/dev/vda")
    /// * `fstype` - Filesystem type (e.g., "ext4") or None for auto-detection
    /// * `options` - Mount options or None for defaults
    ///
    /// # Note
    /// The block device must be configured via `add_disk_with_format` before calling this.
    pub unsafe fn set_root_disk_remount(
        &self,
        device: &str,
        fstype: Option<&str>,
        options: Option<&str>,
    ) -> BoxliteResult<()> {
        tracing::debug!(
            "Setting root disk remount: device={}, fstype={:?}, options={:?}",
            device,
            fstype,
            options
        );

        let device_c = CString::new(device)
            .map_err(|e| BoxliteError::Engine(format!("invalid device path: {e}")))?;

        let fstype_c = fstype
            .map(CString::new)
            .transpose()
            .map_err(|e| BoxliteError::Engine(format!("invalid fstype: {e}")))?;

        let options_c = options
            .map(CString::new)
            .transpose()
            .map_err(|e| BoxliteError::Engine(format!("invalid options: {e}")))?;

        check_status("krun_set_root_disk_remount", unsafe {
            krun_set_root_disk_remount(
                self.ctx_id,
                device_c.as_ptr(),
                fstype_c.as_ref().map_or(ptr::null(), |c| c.as_ptr()),
                options_c.as_ref().map_or(ptr::null(), |c| c.as_ptr()),
            )
        })
    }

    pub unsafe fn set_kernel(
        &self,
        kernel_path: &str,
        kernel_format: u32,
        initramfs: Option<&str>,
        cmdline: Option<&str>,
    ) -> BoxliteResult<()> {
        let kernel_c = CString::new(kernel_path)
            .map_err(|e| BoxliteError::Engine(format!("invalid kernel path: {e}")))?;

        let initramfs_c = if let Some(initramfs) = initramfs {
            Some(
                CString::new(initramfs)
                    .map_err(|e| BoxliteError::Engine(format!("invalid initramfs path: {e}")))?,
            )
        } else {
            None
        };

        let cmdline_c = if let Some(cmdline) = cmdline {
            Some(
                CString::new(cmdline)
                    .map_err(|e| BoxliteError::Engine(format!("invalid cmdline: {e}")))?,
            )
        } else {
            None
        };

        check_status("krun_set_kernel", unsafe {
            krun_set_kernel(
                self.ctx_id,
                kernel_c.as_ptr(),
                kernel_format,
                initramfs_c.as_ref().map_or(ptr::null(), |c| c.as_ptr()),
                cmdline_c.as_ref().map_or(ptr::null(), |c| c.as_ptr()),
            )
        })
    }

    pub unsafe fn set_overlayfs_rootfs(&self, layers: &[String]) -> BoxliteResult<()> {
        tracing::trace!("Setting overlayfs with layers: {:?}", layers);
        // Fallback: use the first layer as rootfs when overlayfs is not available
        if let Some(first_layer) = layers.first() {
            tracing::trace!("Using first layer as rootfs: {}", first_layer);
            unsafe { self.set_rootfs(first_layer) }
        } else {
            Err(BoxliteError::Engine(
                "No layers provided for overlayfs".into(),
            ))
        }
    }

    pub unsafe fn set_exec(
        &self,
        exec: &str,
        args: &[String],
        env: &[(String, String)],
    ) -> BoxliteResult<()> {
        let exec_c = CString::new(exec)
            .map_err(|e| BoxliteError::Engine(format!("invalid exec path: {e}")))?;

        let mut all_args = vec![];
        all_args.extend_from_slice(args);

        tracing::trace!("Building argv array with {} elements:", all_args.len());
        for (i, arg) in all_args.iter().enumerate() {
            tracing::trace!("  argv[{}] = {:?}", i, arg);
        }

        let arg_storage: Vec<CString> = all_args
            .iter()
            .map(|arg| {
                CString::new(arg.as_str())
                    .map_err(|e| BoxliteError::Engine(format!("invalid arg: {e}")))
            })
            .collect::<Result<_, _>>()?;
        let mut arg_ptrs: Vec<*const std::ffi::c_char> =
            arg_storage.iter().map(|arg| arg.as_ptr()).collect();
        arg_ptrs.push(ptr::null());

        tracing::trace!("Building env array with {} elements:", env.len());
        for (k, v) in env.iter() {
            tracing::trace!("  {}={}", k, v);
        }

        let env_storage = Self::env_to_cstring(env)?;
        let mut env_ptrs: Vec<*const std::ffi::c_char> =
            env_storage.iter().map(|entry| entry.as_ptr()).collect();
        env_ptrs.push(ptr::null());

        check_status("krun_set_exec", unsafe {
            krun_set_exec(
                self.ctx_id,
                exec_c.as_ptr(),
                arg_ptrs.as_ptr(),
                env_ptrs.as_ptr(),
            )
        })
    }

    pub unsafe fn set_env(&self, env: &[(String, String)]) -> BoxliteResult<()> {
        if env.is_empty() {
            let empty: [*const std::ffi::c_char; 1] = [ptr::null()];
            return check_status("krun_set_env", unsafe {
                krun_set_env(self.ctx_id, empty.as_ptr())
            });
        }

        let env_storage = Self::env_to_cstring(env)?;
        let mut ptrs: Vec<*const std::ffi::c_char> =
            env_storage.iter().map(|c| c.as_ptr()).collect();
        ptrs.push(ptr::null());

        check_status("krun_set_env", unsafe {
            krun_set_env(self.ctx_id, ptrs.as_ptr())
        })
    }

    fn env_to_cstring(env: &[(String, String)]) -> Result<Vec<CString>, BoxliteError> {
        let entries: Vec<CString> = env
            .iter()
            .map(|(k, v)| {
                CString::new(format!("{}={}", k, v))
                    .map_err(|e| BoxliteError::Engine(format!("invalid env: {e}")))
            })
            .collect::<Result<_, _>>()?;
        Ok(entries)
    }

    pub unsafe fn set_workdir(&self, workdir: &str) -> BoxliteResult<()> {
        let workdir_c = CString::new(workdir)
            .map_err(|e| BoxliteError::Engine(format!("invalid workdir path: {e}")))?;
        check_status("krun_set_workdir", unsafe {
            krun_set_workdir(self.ctx_id, workdir_c.as_ptr())
        })
    }

    pub unsafe fn split_irqchip(&self, enable: bool) -> BoxliteResult<()> {
        tracing::trace!("Setting split IRQ chip to: {}", enable);
        check_status("krun_split_irqchip", unsafe {
            krun_split_irqchip(self.ctx_id, enable)
        })
    }

    pub unsafe fn set_gpu_options(&self, virgl_flags: u32) -> BoxliteResult<()> {
        tracing::trace!("Setting GPU options with virgl_flags: {}", virgl_flags);
        check_status("krun_set_gpu_options", unsafe {
            krun_set_gpu_options(self.ctx_id, virgl_flags)
        })
    }

    pub unsafe fn set_rlimits(&self, rlimits: &[String]) -> BoxliteResult<()> {
        tracing::trace!("Setting rlimits: {:?}", rlimits);
        if rlimits.is_empty() {
            let empty: [*const std::ffi::c_char; 1] = [ptr::null()];
            return check_status("krun_set_rlimits", unsafe {
                krun_set_rlimits(self.ctx_id, empty.as_ptr())
            });
        }

        let entries: Vec<CString> = rlimits
            .iter()
            .map(|rlimit| {
                CString::new(rlimit.as_str())
                    .map_err(|e| BoxliteError::Engine(format!("invalid rlimit: {e}")))
            })
            .collect::<Result<_, _>>()?;
        let mut ptrs: Vec<*const std::ffi::c_char> = entries.iter().map(|c| c.as_ptr()).collect();
        ptrs.push(ptr::null());

        check_status("krun_set_rlimits", unsafe {
            krun_set_rlimits(self.ctx_id, ptrs.as_ptr())
        })
    }

    pub unsafe fn set_port_map(&self, port_map: &[String]) -> BoxliteResult<()> {
        tracing::trace!("Setting port map: {:?}", port_map);
        if port_map.is_empty() {
            let empty: [*const std::ffi::c_char; 1] = [ptr::null()];
            return check_status("krun_set_port_map", unsafe {
                krun_set_port_map(self.ctx_id, empty.as_ptr())
            });
        }

        let entries: Vec<CString> = port_map
            .iter()
            .map(|mapping| {
                CString::new(mapping.as_str())
                    .map_err(|e| BoxliteError::Engine(format!("invalid port mapping: {e}")))
            })
            .collect::<Result<_, _>>()?;
        let mut ptrs: Vec<*const std::ffi::c_char> = entries.iter().map(|c| c.as_ptr()).collect();
        ptrs.push(ptr::null());

        check_status("krun_set_port_map", unsafe {
            krun_set_port_map(self.ctx_id, ptrs.as_ptr())
        })
    }

    pub unsafe fn set_nested_virt(&self, enabled: bool) -> BoxliteResult<()> {
        tracing::trace!("Setting nested virtualization to: {}", enabled);
        check_status("krun_set_nested_virt", unsafe {
            krun_set_nested_virt(self.ctx_id, enabled)
        })
    }

    /// Add a network backend via file descriptor.
    ///
    /// This is used for external network backends like gvproxy or passt that provide
    /// their own file descriptor for communication.
    ///
    /// # Arguments
    /// * `socket_path` - Path to Unix socket for network backend
    /// * `features` - Virtio-net feature flags bitmask
    /// * `connection_type` - Socket type (UnixStream for passt, UnixGram for gvproxy)
    /// * `mac_address` - MAC address for guest network interface (passed from backend)
    pub unsafe fn add_net_path(
        &self,
        socket_path: &str,
        features: u32,
        connection_type: crate::net::ConnectionType,
        mac_address: [u8; 6],
    ) -> BoxliteResult<()> {
        tracing::debug!(socket_path, features, connection_type = ?connection_type, "Adding network backend via socket path");

        // Convert socket path to CString for FFI
        let socket_path_c = CString::new(socket_path)
            .map_err(|e| BoxliteError::Engine(format!("invalid socket path: {e}")))?;

        // Use the appropriate libkrun function based on socket type:
        // - UnixStream for passt/socket_vmnet (SOCK_STREAM)
        // - UnixGram for gvproxy/vmnet-helper (SOCK_DGRAM)
        //
        // IMPORTANT: Pass the socket PATH (not FD) so libkrun can connect itself
        // and send the VFKit magic handshake at the correct time
        match connection_type {
            crate::net::ConnectionType::UnixStream => {
                check_status("krun_add_net_unixstream", unsafe {
                    krun_add_net_unixstream(
                        self.ctx_id,
                        socket_path_c.as_ptr(), // c_path: socket path (let libkrun connect)
                        -1,                     // fd: -1 (use path instead)
                        mac_address.as_ptr(),   // c_mac: valid MAC address (required, not NULL!)
                        features,               // features: virtio-net features bitmask
                        0,                      // flags: 0 for default
                    )
                })
            }
            crate::net::ConnectionType::UnixDgram => {
                check_status("krun_add_net_unixgram", unsafe {
                    krun_add_net_unixgram(
                        self.ctx_id,
                        socket_path_c.as_ptr(), // c_path: socket path (let libkrun connect)
                        -1,                     // fd: -1 (use path instead)
                        mac_address.as_ptr(),   // c_mac: valid MAC address (required, not NULL!)
                        features,               // features: virtio-net features bitmask
                        crate::vmm::krun::constants::network_features::NET_FLAG_VFKIT, // flags: Send VFKIT magic handshake
                    )
                })
            }
        }
    }

    /// Add a virtiofs mount, sharing a host directory with the guest.
    ///
    /// # Arguments
    /// * `host_path` - Path to directory on host to share
    /// * `mount_tag` - Tag used by guest to mount this share (e.g., "layer0", "upper")
    pub unsafe fn add_virtiofs(&self, mount_tag: &str, host_path: &str) -> BoxliteResult<()> {
        tracing::debug!(host_path, mount_tag, "Adding virtiofs mount");

        let host_path_c = CString::new(host_path)
            .map_err(|e| BoxliteError::Engine(format!("invalid host path: {e}")))?;
        let mount_tag_c = CString::new(mount_tag)
            .map_err(|e| BoxliteError::Engine(format!("invalid mount tag: {e}")))?;

        check_status("krun_add_virtiofs", unsafe {
            krun_add_virtiofs(self.ctx_id, mount_tag_c.as_ptr(), host_path_c.as_ptr())
        })
    }

    /// Configure vsock port with Unix socket bridge.
    ///
    /// # Arguments
    /// * `port` - Guest vsock port number
    /// * `socket_path` - Host Unix socket path for the bridge
    /// * `listen` - If true, libkrun creates the socket and listens (host connects)
    pub unsafe fn add_vsock_port(
        &self,
        port: u32,
        socket_path: &str,
        listen: bool,
    ) -> BoxliteResult<()> {
        tracing::debug!(port, socket_path, listen, "Configuring vsock port");
        let socket_path_c = CString::new(socket_path)
            .map_err(|e| BoxliteError::Engine(format!("invalid socket path: {e}")))?;
        check_status("krun_add_vsock_port2", unsafe {
            krun_add_vsock_port2(self.ctx_id, port, socket_path_c.as_ptr(), listen)
        })
    }

    /// Add a disk image with explicit format specification.
    ///
    /// This API supports multiple disk formats (raw, qcow2, etc.) by explicitly
    /// specifying the format. This is safer than auto-probing which can be dangerous.
    ///
    /// # Arguments
    /// * `block_id` - Identifier for the block device (e.g., "vda", "vdb")
    /// * `disk_path` - Path to the disk images file on the host
    /// * `read_only` - Whether to mount the disk as read-only
    /// * `format` - Disk images format: "raw", "qcow2", etc.
    ///
    /// # Security Note
    /// Non-raw images (like qcow2) can reference other files, which libkrun will
    /// automatically open and give the guest access to. Use with caution.
    ///
    /// # Example
    /// ```ignore
    /// // Attach a qcow2 disk images
    /// ctx.add_disk_with_format("vda", "/path/to/disk.qcow2", false, "qcow2")?;
    ///
    /// // Attach a raw disk (same as add_disk but more explicit)
    /// ctx.add_disk_with_format("vdb", "/path/to/disk.raw", true, "raw")?;
    /// ```
    pub unsafe fn add_disk_with_format(
        &self,
        block_id: &str,
        disk_path: &str,
        read_only: bool,
        format: &str,
    ) -> BoxliteResult<()> {
        tracing::debug!(
            block_id,
            disk_path,
            read_only,
            format,
            "Adding disk images with format"
        );

        let block_id_c = CString::new(block_id)
            .map_err(|e| BoxliteError::Engine(format!("invalid block_id: {e}")))?;
        let disk_path_c = CString::new(disk_path)
            .map_err(|e| BoxliteError::Engine(format!("invalid disk path: {e}")))?;

        // Convert format string to libkrun constant
        let disk_format = match format {
            "raw" => libkrun_sys::KRUN_DISK_FORMAT_RAW,
            "qcow2" => libkrun_sys::KRUN_DISK_FORMAT_QCOW2,
            _ => {
                return Err(BoxliteError::Engine(format!(
                    "unsupported disk format: {} (supported: raw, qcow2)",
                    format
                )));
            }
        };

        check_status("krun_add_disk2", unsafe {
            krun_add_disk2(
                self.ctx_id,
                block_id_c.as_ptr(),
                disk_path_c.as_ptr(),
                disk_format,
                read_only,
            )
        })
    }

    /// Set the uid for the microVM process.
    ///
    /// This should be called before `start_enter`.
    pub unsafe fn setuid(&self, uid: libc::uid_t) -> BoxliteResult<()> {
        tracing::debug!(uid, "Setting VM process uid");
        check_status("krun_setuid", unsafe { krun_setuid(self.ctx_id, uid) })
    }

    /// Set the gid for the microVM process.
    ///
    /// This should be called before `start_enter`.
    pub unsafe fn setgid(&self, gid: libc::gid_t) -> BoxliteResult<()> {
        tracing::debug!(gid, "Setting VM process gid");
        check_status("krun_setgid", unsafe { krun_setgid(self.ctx_id, gid) })
    }

    /// Redirect VM console output to a file.
    ///
    /// This allows capturing kernel and init output for debugging.
    /// Must be called before `start_enter`.
    pub unsafe fn set_console_output(&self, filepath: &str) -> BoxliteResult<()> {
        tracing::debug!(filepath, "Setting console output path");
        let filepath_c = CString::new(filepath)
            .map_err(|e| BoxliteError::Engine(format!("invalid console output path: {e}")))?;
        check_status("krun_set_console_output", unsafe {
            krun_set_console_output(self.ctx_id, filepath_c.as_ptr())
        })
    }

    pub unsafe fn start_enter(&self) -> i32 {
        tracing::trace!(ctx_id = self.ctx_id, "Calling krun_start_enter");
        let status = unsafe { krun_start_enter(self.ctx_id) };
        tracing::trace!(status, "krun_start_enter returned");
        if status < 0 {
            tracing::error!(status, "krun_start_enter failed");
        }
        status
    }
}

impl Drop for KrunContext {
    fn drop(&mut self) {
        unsafe {
            let _ = krun_free_ctx(self.ctx_id);
        }
    }
}
