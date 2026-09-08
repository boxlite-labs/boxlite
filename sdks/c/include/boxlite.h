#ifndef BOXLITE_H
#define BOXLITE_H

#pragma once

#include <stdarg.h>
#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include <stdlib.h>

// Maximum number of buffered events before producer tasks yield.
#define QUEUE_CAPACITY 4096

// Error codes returned by BoxLite C API functions.
//
// These codes map directly to Rust's BoxliteError variants,
// allowing programmatic error handling in C.
typedef enum BoxliteErrorCode {
  // Operation succeeded
  Ok = 0,
  // Internal error
  Internal = 1,
  // Resource not found
  NotFound = 2,
  // Resource already exists
  AlreadyExists = 3,
  // Invalid state for operation
  InvalidState = 4,
  // Invalid argument provided
  InvalidArgument = 5,
  // Configuration error
  Config = 6,
  // Storage error
  Storage = 7,
  // Image error
  Image = 8,
  // Network error
  Network = 9,
  // Execution error
  Execution = 10,
  // Resource stopped
  Stopped = 11,
  // Engine error
  Engine = 12,
  // Unsupported operation
  Unsupported = 13,
  // Database error
  Database = 14,
  // Portal/communication error
  Portal = 15,
  // RPC error
  Rpc = 16,
  // RPC transport error
  RpcTransport = 17,
  // Metadata error
  Metadata = 18,
  // Unsupported engine error
  UnsupportedEngine = 19,
  // System resource limit reached
  ResourceExhausted = 20,
  // Interactive execution session was reaped server-side after disconnect.
  // Reattach is no longer possible — start a new exec.
  SessionReaped = 21,
} BoxliteErrorCode;

// Network mode exposed by [`CNetworkInfo`].
typedef enum BoxliteNetworkMode {
  BoxliteNetworkModeEnabled = 0,
  BoxliteNetworkModeDisabled = 1,
} BoxliteNetworkMode;

// Transport protocol for a port forwarding rule.
typedef enum BoxlitePortProtocol {
  BoxlitePortProtocolTcp = 0,
  BoxlitePortProtocolUdp = 1,
} BoxlitePortProtocol;

typedef enum BoxliteRegistryTransport {
  BoxliteRegistryTransportHttps = 0,
  BoxliteRegistryTransportHttp = 1,
} BoxliteRegistryTransport;

// Streaming-copy source shape. For copy-in, `BoxliteCopySourceKindUnknown`
// means the caller cannot tell and the guest peeks at the archive. For
// copy-out, it means the peer omitted the hint. The C-ABI mirror of the core
// `CopySourceKind`.
typedef enum BoxliteCopySourceKind {
  BoxliteCopySourceKindUnknown = 0,
  BoxliteCopySourceKindFile = 1,
  BoxliteCopySourceKindDir = 2,
} BoxliteCopySourceKind;

// Opaque handle wrapping an `AdvancedBoxOptions`. Allocated via
// `boxlite_advanced_options_new`, freed via `boxlite_advanced_options_free`.
typedef struct AdvancedBoxOptionsHandle AdvancedBoxOptionsHandle;

// Opaque handle to a running box.
//
// `handle` is wrapped in `Arc` so it can be cloned into Tokio tasks for
// async lifecycle ops.
typedef struct BoxHandle BoxHandle;

// Opaque handle for network operations on a box.
typedef struct BoxNetworkHandle BoxNetworkHandle;

// Opaque handle for Runner API (auto-manages runtime)
typedef struct BoxRunner BoxRunner;

// Opaque handle for a one-shot prepared tunnel.
typedef struct BoxTunnelHandle BoxTunnelHandle;

// Opaque handle for a streaming copy-in (push archive bytes into the guest).
//
// The lifecycle is fixed, and the third step is not optional:
//
//     boxlite_copy_in_start
//       boxlite_copy_in_write   (repeat)
//       boxlite_copy_in_close   on success
//       boxlite_copy_in_abort   when the source read failed
//     boxlite_copy_in_free
//
// Going straight from write to free is what makes a truncated upload
// dangerous: freeing the handle drops the channel, which the guest reads
// as a clean EOF and commits. Only boxlite_copy_in_abort turns a
// mid-transfer source failure into a terminal error the guest can refuse.
typedef struct CBoxCopyInStream CBoxCopyInStream;

// Opaque handle for a streaming copy-out (pull archive bytes from the guest).
typedef struct CBoxCopyOutStream CBoxCopyOutStream;

// Opaque credential handle. Wraps a core `Arc<dyn Credential>` so the
// concrete credential kind (today only `ApiKeyCredential`) is hidden
// behind one C type, matching the trait/interface surface in the other
// SDKs.
typedef struct CredentialHandle CredentialHandle;

// Opaque handle to a running command execution.
typedef struct ExecutionHandle ExecutionHandle;

// Opaque handle to runtime image operations.
typedef struct ImageHandle ImageHandle;

typedef struct OptionsHandle OptionsHandle;

// Opaque REST options handle. Owns a core [`BoxliteRestOptions`] that
// the setters mutate in place before construction.
typedef struct RestOptionsHandle RestOptionsHandle;

// Opaque handle to a BoxliteRuntime instance with its Tokio runtime and the
// per-runtime event queue used by the post-and-drain callback API.
typedef struct RuntimeHandle RuntimeHandle;

// Opaque long-lived listener handle.
typedef struct TunnelForwarderHandle TunnelForwarderHandle;

// Opaque handle to runtime named-volume operations.
//
// The handle owns a cloneable core volume handle plus the runtime liveness,
// Tokio runtime, and event queue needed to submit asynchronous work. C callers
// receive this as an opaque `CBoxliteVolumeHandle` and must release it with
// `boxlite_volume_free`.
typedef struct VolumeHandle VolumeHandle;

typedef struct AdvancedBoxOptionsHandle CAdvancedBoxOptions;

// Extended error information for C API.
//
// Contains both an error code (for programmatic handling)
// and an optional detailed message (for debugging).
typedef struct FFIError {
  // Error code
  enum BoxliteErrorCode code;
  // Detailed error message (NULL if none, caller must free with boxlite_error_free)
  char *message;
} FFIError;

typedef struct BoxHandle CBoxHandle;

typedef struct FFIError CBoxliteError;

// Box export completion.
typedef void (*CBoxExportCb)(char*, CBoxliteError*, void*);

typedef struct RuntimeHandle CBoxliteRuntime;

// Runtime import completion.
typedef void (*CRuntimeImportCb)(CBoxHandle*, CBoxliteError*, void*);

typedef struct OptionsHandle CBoxliteOptions;

// Box creation completion.
typedef void (*CBoxCreateBoxCb)(CBoxHandle*, CBoxliteError*, void*);

// Get-or-create completion. Same shape as create plus a `bool` that is `true`
// when a new box was created and `false` when an existing box was adopted.
typedef void (*CBoxGetOrCreateBoxCb)(CBoxHandle*, bool, CBoxliteError*, void*);

// Box stop completion.
typedef void (*CBoxStopBoxCb)(CBoxliteError*, void*);

// Box attach (get) completion.
typedef void (*CBoxGetBoxCb)(CBoxHandle*, CBoxliteError*, void*);

// Box remove completion.
typedef void (*CBoxRemoveBoxCb)(CBoxliteError*, void*);

// Box start completion.
typedef void (*CBoxStartBoxCb)(CBoxliteError*, void*);

// Copy (into / out of) completion.
typedef void (*CBoxCopyCb)(CBoxliteError*, void*);

// C-compatible command descriptor with all BoxCommand options.
//
// All string fields are nullable — NULL means "use default".
// `timeout_secs` of 0.0 means no timeout.
typedef struct BoxliteCommand {
  // Command to execute (required, must not be NULL).
  const char *command;
  // Array of argument strings. NULL = no args.
  const char *const *args;
  // Number of arguments in `args`.
  int argc;
  // Array of env var pairs: [key0, val0, key1, ...]. NULL = inherit env.
  const char *const *env_pairs;
  // Number of strings in `env_pairs`; odd trailing values are ignored.
  int env_count;
  // Working directory inside the container. NULL = container default.
  const char *workdir;
  // User spec (e.g., "nobody", "1000:1000"). NULL = container default.
  const char *user;
  // Timeout in seconds. 0.0 = no timeout.
  double timeout_secs;
  // Enable TTY mode for interactive programs.
  int tty;
} BoxliteCommand;

typedef struct ExecutionHandle CExecutionHandle;

// Streaming stdout chunk callback.
typedef void (*CBoxStdoutCb)(const uint8_t*, size_t, void*);

// Streaming stderr chunk callback.
typedef void (*CBoxStderrCb)(const uint8_t*, size_t, void*);

// Process exit callback (fired once per execution).
typedef void (*CBoxExitCb)(int, void*);

// Execution wait completion (carries exit code on success).
typedef void (*CExecutionWaitCb)(int, CBoxliteError*, void*);

// Execution kill completion.
typedef void (*CExecutionKillCb)(CBoxliteError*, void*);

// Execution signal completion. Distinct typedef from `CExecutionKillCb`
// even though the shape is identical so callers can route SIGKILL (kill)
// and arbitrary-signal (signal) callbacks to different handlers without
// relying on positional inference.
typedef void (*CExecutionSignalCb)(CBoxliteError*, void*);

// Execution PTY resize completion.
typedef void (*CExecutionResizeCb)(CBoxliteError*, void*);

typedef struct BoxRunner CBoxliteSimple;

// Result structure for runner command execution
typedef struct ExecResult {
  int exit_code;
  char *stdout_text;
  char *stderr_text;
} ExecResult;

typedef struct ExecResult CBoxliteExecResult;

typedef struct ImageHandle CBoxliteImageHandle;

typedef struct CImagePullResult {
  char *reference;
  char *config_digest;
  int layer_count;
} CImagePullResult;

// Image pull completion.
typedef void (*CBoxImagePullCb)(struct CImagePullResult*, CBoxliteError*, void*);

typedef struct CImageInfo {
  char *reference;
  char *repository;
  char *tag;
  char *id;
  int64_t cached_at;
  uint64_t size;
  int has_size;
} CImageInfo;

typedef struct CImageInfoList {
  struct CImageInfo *items;
  int count;
} CImageInfoList;

// Image list completion.
typedef void (*CBoxImageListCb)(struct CImageInfoList*, CBoxliteError*, void*);

// A concrete host listener published to a guest port.
//
// `host_ip` is owned by the enclosing [`CBoxInfo`].
typedef struct CPublishedPort {
  uint16_t guest_port;
  char *host_ip;
  uint16_t host_port;
  enum BoxlitePortProtocol protocol;
} CPublishedPort;

// Owned list of concrete published ports.
//
// A non-null list with `count == 0` means publication metadata was resolved
// and no listeners are active. The list is owned by its enclosing
// [`CNetworkInfo`].
typedef struct CPublishedPortList {
  struct CPublishedPort *items;
  int count;
} CPublishedPortList;

// Outbound (guest → internet) network mode and allowlist.
// `allow_net` points to `allow_net_count` owned strings, owned by the
// enclosing [`CNetworkInfo`].
typedef struct COutboundNetworkInfo {
  enum BoxliteNetworkMode mode;
  char **allow_net;
  int allow_net_count;
} COutboundNetworkInfo;

// Inbound (internet → guest) network mode and allowlist.
// `allow_net` points to `allow_net_count` owned strings, owned by the
// enclosing [`CNetworkInfo`].
typedef struct CInboundNetworkInfo {
  enum BoxliteNetworkMode mode;
  char **allow_net;
  int allow_net_count;
} CInboundNetworkInfo;

// Typed network metadata owned by an enclosing [`CBoxInfo`].
//
// `published_ports` is null when the current handle does not know the
// bindings, non-null and empty when there are no active publications, and
// otherwise contains concrete bindings.
// The first four fields are byte-compatible with the pre-split struct
// (`mode`, `allow_net`, `allow_net_count`, `published_ports`), so callers
// compiled against the old header keep reading valid data at the same
// offsets. They alias `outbound`'s allocations — never free them separately;
// [`free_network_info`] releases each allocation exactly once through
// `outbound`/`inbound`.
typedef struct CNetworkInfo {
  // Deprecated: read `outbound.mode`. Mirrors it for old callers.
  enum BoxliteNetworkMode mode;
  // Deprecated: read `outbound.allow_net`. Aliases it — do not free.
  char **allow_net;
  // Deprecated: read `outbound.allow_net_count`.
  int allow_net_count;
  struct CPublishedPortList *published_ports;
  struct COutboundNetworkInfo outbound;
  struct CInboundNetworkInfo inbound;
} CNetworkInfo;

typedef struct CBoxInfo {
  char *id;
  char *name;
  char *image;
  char *status;
  int running;
  int pid;
  int cpus;
  int memory_mib;
  uint32_t auto_stop;
  uint32_t auto_delete;
  int auto_resume;
  int64_t created_at;
  // Owned typed network metadata; null when network metadata is unavailable.
  struct CNetworkInfo *network;
  // Unix milliseconds when the box most recently entered `Running`; `0`
  // when no start time was recorded. Preserved after stop or reboot; when
  // [`Self::pid`] is nonzero, the timestamp describes that live PID.
  // Milliseconds — not `created_at`'s seconds — preserve sub-second ordering
  // against a job's timeline.
  int64_t started_at;
  // Unix milliseconds of the box's last recorded activity — the clock
  // AutoStop measures idleness against; `0` when nothing was recorded, which
  // is always the case for local runtimes.
  int64_t last_activity_at;
} CBoxInfo;

// Box info completion. On success the callback owns the non-null metadata and
// must release it with `boxlite_free_box_info`. The error pointer is borrowed
// for callback dispatch only; on failure the metadata pointer is null.
typedef void (*CBoxInfoCb)(struct CBoxInfo*, CBoxliteError*, void*);

typedef struct CBoxInfoList {
  struct CBoxInfo *items;
  int count;
} CBoxInfoList;

// Box info list completion.
typedef void (*CBoxInfoListCb)(struct CBoxInfoList*, CBoxliteError*, void*);

typedef struct CBoxMetrics {
  double cpu_percent;
  int64_t memory_bytes;
  int commands_executed;
  int exec_errors;
  int64_t bytes_sent;
  int64_t bytes_received;
  int64_t create_duration_ms;
  int64_t boot_duration_ms;
  int64_t network_bytes_sent;
  int64_t network_bytes_received;
  int network_tcp_connections;
  int network_tcp_errors;
} CBoxMetrics;

// Per-box metrics completion.
typedef void (*CBoxMetricsCb)(struct CBoxMetrics*, CBoxliteError*, void*);

typedef struct CRuntimeMetrics {
  int boxes_created_total;
  int boxes_failed_total;
  int num_running_boxes;
  int total_commands_executed;
  int total_exec_errors;
} CRuntimeMetrics;

// Runtime metrics completion.
typedef void (*CRuntimeMetricsCb)(struct CRuntimeMetrics*, CBoxliteError*, void*);

typedef struct BoxNetworkHandle CBoxNetworkHandle;

typedef struct TunnelForwarderHandle CTunnelForwarderHandle;

// Tunnel forwarder wait completion.
typedef void (*CTunnelForwarderWaitCb)(CBoxliteError*, void*);

// Tunnel forwarder close completion.
typedef void (*CTunnelForwarderCloseCb)(CBoxliteError*, void*);

typedef struct BoxTunnelHandle CBoxTunnelHandle;

typedef uint32_t BoxliteSocketAddressKind;

// Listener address. Input strings are borrowed for the duration of a call.
typedef struct BoxliteSocketAddress {
  BoxliteSocketAddressKind kind;
  const char *host;
  uint16_t port;
  const char *path;
} BoxliteSocketAddress;

typedef struct CredentialHandle CBoxliteCredential;

typedef struct RestOptionsHandle CBoxliteRestOptions;

typedef struct BoxliteImageRegistry {
  const char *host;
  enum BoxliteRegistryTransport transport;
  int skip_verify;
  int search;
  const char *username;
  const char *password;
  const char *bearer_token;
} BoxliteImageRegistry;

typedef struct VolumeHandle CBoxliteVolumeHandle;

// Runtime shutdown completion.
typedef void (*CRuntimeShutdownCb)(CBoxliteError*, void*);

// C ABI representation of volume metadata.
//
// `id` and `created_at` are non-null, heap-owned C strings. A standalone value
// is transferred to the callback and must be released exactly once with
// `boxlite_free_volume_info`. List entries remain owned by their enclosing
// [`CVolumeInfoList`] and must not be freed individually. `size_bytes` is
// meaningful only when `has_size` is non-zero.
typedef struct CVolumeInfo {
  char *id;
  // Volume name, mountable in place of the id. Defaults to the id.
  char *name;
  char *created_at;
  uint64_t size_bytes;
  int has_size;
} CVolumeInfo;

// Volume create completion.
//
// On success the callback takes ownership of the non-null metadata pointer and
// must release it with `boxlite_free_volume_info`. On failure the pointer is
// null. The error pointer is borrowed for callback dispatch only.
typedef void (*CBoxVolumeCreateCb)(struct CVolumeInfo*, CBoxliteError*, void*);

// C ABI representation of a volume metadata list.
//
// `items` points to `count` contiguous [`CVolumeInfo`] entries and may be null
// only when `count` is zero. The callback recipient owns the list, its array,
// and all entry strings and must release them together with
// `boxlite_free_volume_info_list`.
typedef struct CVolumeInfoList {
  struct CVolumeInfo *items;
  int count;
} CVolumeInfoList;

// Volume list completion. A successful callback owns the list and must release
// it with `boxlite_free_volume_info_list`; the error pointer is callback-scoped.
typedef void (*CBoxVolumeListCb)(struct CVolumeInfoList*, CBoxliteError*, void*);

// Volume get completion with the same ownership contract as volume create.
typedef void (*CBoxVolumeGetCb)(struct CVolumeInfo*, CBoxliteError*, void*);

// Volume remove completion. The error pointer is borrowed for callback dispatch
// only and no result allocation is produced.
typedef void (*CBoxVolumeRemoveCb)(CBoxliteError*, void*);

#define BoxliteSocketTcp 0

#define BoxliteSocketUnix 1

#ifdef __cplusplus
extern "C" {
#endif // __cplusplus

// Allocate a `CAdvancedBoxOptions` initialized to `AdvancedBoxOptions::default()`
// (secure-by-default security profile, mount isolation off, no health check).
//
// Sets `*out_opts` to the new handle on `Ok`. The caller owns the handle and
// must release it via `boxlite_advanced_options_free` once it has been applied
// to a `CBoxliteOptions` via `boxlite_options_set_advanced` (or if no longer
// needed).
enum BoxliteErrorCode boxlite_advanced_options_new(CAdvancedBoxOptions **out_opts,
                                                   struct FFIError *out_error);

// Release a `CAdvancedBoxOptions` previously returned by
// `boxlite_advanced_options_new`. Null is a no-op.
void boxlite_advanced_options_free(CAdvancedBoxOptions *opts);

// Toggle the box's sandbox on the advanced options. `enabled` != 0 selects the
// fully-isolated profile (`SecurityOptions::enabled()`, also the default when
// this is never called); 0 selects `SecurityOptions::disabled()` (master
// switch off, every sub-protection off — for debugging or environments that
// genuinely can't sandbox). Null `opts` is a no-op.
void boxlite_advanced_options_set_security_enabled(CAdvancedBoxOptions *opts, int enabled);

// Replace the capabilities added to BoxLite's Docker-compatible baseline.
//
// A zero count clears the list. Negative counts, null handles, null arrays
// with a positive count, null elements, and invalid UTF-8 fail closed.
enum BoxliteErrorCode boxlite_advanced_options_set_capabilities_add(CAdvancedBoxOptions *opts,
                                                                    const char *const *capabilities,
                                                                    int count);

// Replace the capabilities removed from the container capability set.
//
// A zero count clears the list. Negative counts, null handles, null arrays
// with a positive count, null elements, and invalid UTF-8 fail closed.
enum BoxliteErrorCode boxlite_advanced_options_set_capabilities_drop(CAdvancedBoxOptions *opts,
                                                                     const char *const *capabilities,
                                                                     int count);

// Submit a box export.
//
// On success, the callback owns the returned path and must release it with
// `boxlite_free_string`. The archive itself is never deleted by this bridge.
enum BoxliteErrorCode boxlite_box_export(CBoxHandle *handle,
                                         const char *dest,
                                         CBoxExportCb cb,
                                         void *user_data,
                                         CBoxliteError *out_error);

// Submit a trusted archive import.
//
// A null or empty `name_or_null` leaves the new box unnamed. The callback
// owns the returned stopped box handle. The caller retains ownership of the
// archive file; this bridge never removes it.
enum BoxliteErrorCode boxlite_runtime_import(CBoxliteRuntime *runtime,
                                             const char *archive_path,
                                             const char *name_or_null,
                                             CRuntimeImportCb cb,
                                             void *user_data,
                                             CBoxliteError *out_error);

enum BoxliteErrorCode boxlite_create_box(CBoxliteRuntime *runtime,
                                         CBoxliteOptions *opts,
                                         CBoxCreateBoxCb cb,
                                         void *user_data,
                                         CBoxliteError *out_error);

// Get an existing box by name, or create a new one if it does not exist.
//
// When a box with the given name already exists it returns that box instead
// of failing with "already exists". The callback receives an extra `created`
// flag: `true` when a new box was created, `false` when an existing box was
// adopted — letting callers distinguish the two (e.g. skip re-initialization
// for an adopted box).
enum BoxliteErrorCode boxlite_get_or_create_box(CBoxliteRuntime *runtime,
                                                CBoxliteOptions *opts,
                                                CBoxGetOrCreateBoxCb cb,
                                                void *user_data,
                                                CBoxliteError *out_error);

enum BoxliteErrorCode boxlite_stop_box(CBoxHandle *handle,
                                       CBoxStopBoxCb cb,
                                       void *user_data,
                                       CBoxliteError *out_error);

enum BoxliteErrorCode boxlite_get(CBoxliteRuntime *runtime,
                                  const char *id_or_name,
                                  CBoxGetBoxCb cb,
                                  void *user_data,
                                  CBoxliteError *out_error);

enum BoxliteErrorCode boxlite_remove(CBoxliteRuntime *runtime,
                                     const char *id_or_name,
                                     int force,
                                     CBoxRemoveBoxCb cb,
                                     void *user_data,
                                     CBoxliteError *out_error);

enum BoxliteErrorCode boxlite_start_box(CBoxHandle *handle,
                                        CBoxStartBoxCb cb,
                                        void *user_data,
                                        CBoxliteError *out_error);

char *boxlite_box_id(CBoxHandle *handle);

void boxlite_box_free(CBoxHandle *handle);

enum BoxliteErrorCode boxlite_copy_into(CBoxHandle *handle,
                                        const char *host_src,
                                        const char *guest_dst,
                                        CBoxCopyCb cb,
                                        void *user_data,
                                        CBoxliteError *out_error);

enum BoxliteErrorCode boxlite_copy_out(CBoxHandle *handle,
                                       const char *guest_src,
                                       const char *host_dst,
                                       CBoxCopyCb cb,
                                       void *user_data,
                                       CBoxliteError *out_error);

// Begin downloading `guest_src` as a pull-based raw archive stream.
//
// This call blocks until the stream and its optional source-shape hint are
// ready. On success the returned handle must be released with
// [`boxlite_copy_out_free`]. A non-null `out_source_kind` is initialized to
// `BoxliteCopySourceKindUnknown` and updated to the `...File` or `...Dir`
// variant when the peer supplies the hint.
struct CBoxCopyOutStream *boxlite_copy_out_start(CBoxHandle *handle,
                                                 const char *guest_src,
                                                 int32_t *out_source_kind,
                                                 CBoxliteError *out_error);

// Read the next raw archive bytes from a copy-out stream.
//
// This call blocks while the upstream stream is pending. `Ok` with a
// positive `out_read` returns data; `Ok` with zero length is sticky EOF. A
// stream item error is terminal: its first read reports the stream error and
// later reads return `InvalidState` without polling upstream again.
enum BoxliteErrorCode boxlite_copy_out_read(struct CBoxCopyOutStream *stream,
                                            uint8_t *buffer,
                                            size_t capacity,
                                            size_t *out_read,
                                            CBoxliteError *out_error);

// Reclaim a copy-out stream handle. A null handle is a no-op.
//
// The caller must not invoke [`boxlite_copy_out_read`] concurrently or race
// a read with this function.
void boxlite_copy_out_free(struct CBoxCopyOutStream *stream);

// Begin a streaming copy-in, returning an opaque transfer handle.
//
// `source_kind` describes the archive shape: `BoxliteCopySourceKind`'s
// discriminant (`...Unknown`=0, `...File`=1, `...Dir`=2), or 0 when the
// caller cannot tell (older clients) — the guest then peeks the archive to
// decide.
// Taken as an integer because C callers can pass any value, and
// out-of-range discriminants must behave as Unknown rather than as an
// invalid Rust enum.
struct CBoxCopyInStream *boxlite_copy_in_start(CBoxHandle *handle,
                                               const char *guest_dst,
                                               int32_t source_kind,
                                               CBoxCopyCb copy_cb,
                                               void *user_data,
                                               CBoxliteError *out_error);

// Push a chunk of archive bytes into the guest. Blocks when the guest is
// slow (bounded-channel backpressure).
enum BoxliteErrorCode boxlite_copy_in_write(struct CBoxCopyInStream *stream,
                                            const uint8_t *data,
                                            size_t len,
                                            CBoxliteError *out_error);

// Close the copy-in stream, signalling EOF to the guest. Idempotent.
enum BoxliteErrorCode boxlite_copy_in_close(struct CBoxCopyInStream *stream,
                                            CBoxliteError *out_error);

// Abort the copy-in stream: deliver a terminal error to the guest and then
// close the channel. Unlike [`boxlite_copy_in_close`], the guest sees a
// failed stream — a truncated upload can never pass as a clean EOF. Call
// this when the source read failed mid-transfer. This is a one-shot
// transition: after the first call (abort or close), later abort calls return
// `InvalidState`.
enum BoxliteErrorCode boxlite_copy_in_abort(struct CBoxCopyInStream *stream,
                                            CBoxliteError *out_error);

// Reclaim a copy-in stream handle.
//
// Freeing a stream that was never closed or aborted still drops the
// channel, so it signals EOF exactly as boxlite_copy_in_close would — the
// guest commits whatever it received. Call boxlite_copy_in_abort first
// whenever the transfer did not complete; see CBoxCopyInStream for the
// full sequence.
void boxlite_copy_in_free(struct CBoxCopyInStream *stream);

void boxlite_error_free(CBoxliteError *error);

enum BoxliteErrorCode boxlite_box_exec(CBoxHandle *handle,
                                       const struct BoxliteCommand *cmd,
                                       CExecutionHandle **out_execution,
                                       CBoxliteError *out_error);

enum BoxliteErrorCode boxlite_execution_on_stdout(CExecutionHandle *execution,
                                                  CBoxStdoutCb cb,
                                                  void *user_data,
                                                  CBoxliteError *out_error);

enum BoxliteErrorCode boxlite_execution_on_stderr(CExecutionHandle *execution,
                                                  CBoxStderrCb cb,
                                                  void *user_data,
                                                  CBoxliteError *out_error);

enum BoxliteErrorCode boxlite_execution_on_exit(CExecutionHandle *execution,
                                                CBoxExitCb cb,
                                                void *user_data,
                                                CBoxliteError *out_error);

enum BoxliteErrorCode boxlite_execution_stdin_write(CExecutionHandle *execution,
                                                    const uint8_t *data,
                                                    size_t len,
                                                    CBoxliteError *out_error);

// Close the execution's stdin stream, signaling EOF to the guest process.
//
// Synchronous and idempotent: dropping the stdin sender closes the underlying
// mpsc channel; subsequent writes return `InvalidState`; a second close is a
// no-op. Used by clients that want to terminate input without killing the
// process (e.g. `cat`/`wc`/`sort` waiting on stdin EOF).
enum BoxliteErrorCode boxlite_execution_stdin_close(CExecutionHandle *execution,
                                                    CBoxliteError *out_error);

enum BoxliteErrorCode boxlite_execution_wait(CExecutionHandle *execution,
                                             CExecutionWaitCb cb,
                                             void *user_data,
                                             CBoxliteError *out_error);

enum BoxliteErrorCode boxlite_execution_kill(CExecutionHandle *execution,
                                             CExecutionKillCb cb,
                                             void *user_data,
                                             CBoxliteError *out_error);

// Send an arbitrary Unix signal to the execution. `sig` is the signal
// number (e.g. 2 = SIGINT, 15 = SIGTERM). `boxlite_execution_kill`
// remains the dedicated SIGKILL+evict entrypoint; this function is for
// graceful and non-terminal signals (HUP/INT/TERM/WINCH/...) that should
// not tear down the per-execution bookkeeping.
enum BoxliteErrorCode boxlite_execution_signal(CExecutionHandle *execution,
                                               int sig,
                                               CExecutionSignalCb cb,
                                               void *user_data,
                                               CBoxliteError *out_error);

enum BoxliteErrorCode boxlite_execution_tty_resize(CExecutionHandle *execution,
                                                   int rows,
                                                   int cols,
                                                   CExecutionResizeCb cb,
                                                   void *user_data,
                                                   CBoxliteError *out_error);

void boxlite_execution_free(CExecutionHandle *execution);

enum BoxliteErrorCode boxlite_simple_new(const char *image,
                                         int cpus,
                                         int memory_mib,
                                         CBoxliteSimple **out_box,
                                         CBoxliteError *out_error);

enum BoxliteErrorCode boxlite_simple_run(CBoxliteSimple *box_runner,
                                         const char *command,
                                         const char *const *args,
                                         int argc,
                                         CBoxliteExecResult **out_result,
                                         CBoxliteError *out_error);

void boxlite_simple_free(CBoxliteSimple *box_runner);

void boxlite_result_free(CBoxliteExecResult *result);

enum BoxliteErrorCode boxlite_image_pull(CBoxliteImageHandle *handle,
                                         const char *image_ref,
                                         CBoxImagePullCb cb,
                                         void *user_data,
                                         CBoxliteError *out_error);

enum BoxliteErrorCode boxlite_image_list(CBoxliteImageHandle *handle,
                                         CBoxImageListCb cb,
                                         void *user_data,
                                         CBoxliteError *out_error);

void boxlite_image_free(CBoxliteImageHandle *handle);

void boxlite_free_image_info_list(struct CImageInfoList *list);

void boxlite_free_image_pull_result(struct CImagePullResult *result);

enum BoxliteErrorCode boxlite_box_info(CBoxHandle *handle,
                                       CBoxInfoCb cb,
                                       void *user_data,
                                       CBoxliteError *out_error);

enum BoxliteErrorCode boxlite_get_info(CBoxliteRuntime *runtime,
                                       const char *id_or_name,
                                       CBoxInfoCb cb,
                                       void *user_data,
                                       CBoxliteError *out_error);

enum BoxliteErrorCode boxlite_list_info(CBoxliteRuntime *runtime,
                                        CBoxInfoListCb cb,
                                        void *user_data,
                                        CBoxliteError *out_error);

void boxlite_free_box_info(struct CBoxInfo *info);

void boxlite_free_box_info_list(struct CBoxInfoList *list);

enum BoxliteErrorCode boxlite_box_metrics(CBoxHandle *handle,
                                          CBoxMetricsCb cb,
                                          void *user_data,
                                          CBoxliteError *out_error);

enum BoxliteErrorCode boxlite_runtime_metrics(CBoxliteRuntime *runtime,
                                              CRuntimeMetricsCb cb,
                                              void *user_data,
                                              CBoxliteError *out_error);

// Borrow the box's network capability into a new owned handle.
//
// On success, `*out_network` must be released with `boxlite_network_free`.
// Returns `InvalidArgument` for null input/output pointers and writes details
// to `out_error` when provided.
enum BoxliteErrorCode boxlite_box_network(CBoxHandle *handle,
                                          CBoxNetworkHandle **out_network,
                                          CBoxliteError *out_error);

// Return a newly allocated canonical address string.
enum BoxliteErrorCode boxlite_tunnel_forwarder_address(CTunnelForwarderHandle *forwarder,
                                                       char **out_address,
                                                       CBoxliteError *out_error);

enum BoxliteErrorCode boxlite_tunnel_forwarder_wait(CTunnelForwarderHandle *forwarder,
                                                    CTunnelForwarderWaitCb cb,
                                                    void *user_data,
                                                    CBoxliteError *out_error);

enum BoxliteErrorCode boxlite_tunnel_forwarder_close(CTunnelForwarderHandle *forwarder,
                                                     CTunnelForwarderCloseCb cb,
                                                     void *user_data,
                                                     CBoxliteError *out_error);

// Initiate non-blocking cancellation and release the caller's handle.
void boxlite_tunnel_forwarder_free(CTunnelForwarderHandle *forwarder);

// Release a network handle. Accepts NULL and does not affect the box handle.
void boxlite_network_free(CBoxNetworkHandle *network);

// Prepare a one-shot tunnel to `port` in the box.
//
// On success, `*out_tunnel` owns a handle that must be released with
// `boxlite_tunnel_free`. Returns `InvalidArgument` for a null network/output
// pointer or port zero, with details written to `out_error` when provided.
enum BoxliteErrorCode boxlite_network_tunnel(CBoxNetworkHandle *network,
                                             uint16_t port,
                                             CBoxTunnelHandle **out_tunnel,
                                             CBoxliteError *out_error);

// Release an unconsumed tunnel. Existing connections and forwarders remain alive.
void boxlite_tunnel_free(CBoxTunnelHandle *tunnel);

// Read the public URL of a remotely served tunnel, without consuming it.
//
// On success `*out_uri` is an allocated string the caller must release with
// `boxlite_free_string`, or NULL for a local tunnel — a local descriptor is
// already a live connection, so it has no address; use
// `boxlite_tunnel_connect` for those. Errors are returned as a
// `BoxliteErrorCode` and described through `out_error` when provided.
enum BoxliteErrorCode boxlite_tunnel_uri(CBoxTunnelHandle *tunnel,
                                         char **out_uri,
                                         CBoxliteError *out_error);

// Consume the tunnel and return its owned file descriptor.
//
// On success, the caller owns `*out_fd` and must close it.
// On failure `*out_fd` remains -1 and `out_error`
// receives details when provided.
enum BoxliteErrorCode boxlite_tunnel_connect(CBoxTunnelHandle *tunnel,
                                             int32_t *out_fd,
                                             CBoxliteError *out_error);

// Bind a local listener synchronously and start forwarding accepted clients.
enum BoxliteErrorCode boxlite_tunnel_forward(CBoxTunnelHandle *tunnel,
                                             const struct BoxliteSocketAddress *listen,
                                             CTunnelForwarderHandle **out_forwarder,
                                             CBoxliteError *out_error);

enum BoxliteErrorCode boxlite_options_new(const char *image,
                                          CBoxliteOptions **out_opts,
                                          CBoxliteError *out_error);

void boxlite_options_set_rootfs_path(CBoxliteOptions *opts, const char *path);

void boxlite_options_set_name(CBoxliteOptions *opts, const char *name);

void boxlite_options_set_cpus(CBoxliteOptions *opts, int cpus);

void boxlite_options_set_memory(CBoxliteOptions *opts, int memory_mib);

void boxlite_options_set_disk_size_gb(CBoxliteOptions *opts, int disk_size_gb);

void boxlite_options_set_workdir(CBoxliteOptions *opts, const char *workdir);

void boxlite_options_set_user(CBoxliteOptions *opts, const char *user);

void boxlite_options_add_env(CBoxliteOptions *opts, const char *key, const char *val);

// Bind a host directory or file into the box.
//
// Host bind mounts are local-runtime only; a REST runtime rejects them at
// create. Use [`boxlite_options_add_managed_volume`] against a REST runtime.
void boxlite_options_add_bind_mount(CBoxliteOptions *opts,
                                    const char *host_path,
                                    const char *guest_path,
                                    int read_only);

// Mount a managed volume, addressed by its server-assigned id **or** by its
// name — the server resolves either.
//
// `managed_volume` is the volume's id or name (`"my-data"`, `"vol_01K2…"`).
// A local runtime resolves it against its own volume store when the box is
// created; a REST runtime forwards it to the server as-is.
//
// A NULL `opts`, `managed_volume`, or `guest_path` is ignored, matching
// [`boxlite_options_add_bind_mount`].
void boxlite_options_add_managed_volume(CBoxliteOptions *opts,
                                        const char *managed_volume,
                                        const char *guest_path,
                                        int read_only);

// Forward `host_port` on the host to `guest_port` inside the box.
//
// - `host_port`: 0 = let the OS select an available host port.
// - `guest_port`: required, 1-65535.
// - `host_ip`: bind address; NULL or "" = all host interfaces.
//
// Returns `InvalidArgument` if `opts` is NULL, `guest_port` is 0, or
// `host_ip` is not valid UTF-8.
enum BoxliteErrorCode boxlite_options_add_port(CBoxliteOptions *opts,
                                               uint16_t host_port,
                                               uint16_t guest_port,
                                               enum BoxlitePortProtocol protocol,
                                               const char *host_ip);

void boxlite_options_set_network_enabled(CBoxliteOptions *opts);

void boxlite_options_set_network_disabled(CBoxliteOptions *opts);

// Adds one entry to the outbound allowlist. Patterns: exact host,
// "*.example.com", IP, or CIDR.
//
// A non-empty allowlist restricts both TCP and UDP egress. Hostname entries
// are enforced by TLS SNI / HTTP Host inspection, which only TCP carries, so
// an allowlist holding only hostnames denies all UDP egress — add the IP or
// CIDR to keep UDP open.
void boxlite_options_add_network_allow(CBoxliteOptions *opts, const char *host);

// Marks services the box exposes as publicly reachable (the default).
// Mirrors `boxlite_options_set_network_enabled` for the inbound direction.
void boxlite_options_set_network_inbound_enabled(CBoxliteOptions *opts);

// Marks services the box exposes as private — unreachable from outside the
// box. Mirrors `boxlite_options_set_network_disabled` for the inbound
// direction.
void boxlite_options_set_network_inbound_disabled(CBoxliteOptions *opts);

void boxlite_options_add_secret(CBoxliteOptions *opts,
                                const char *name,
                                const char *value,
                                const char *placeholder,
                                const char *const *hosts,
                                int hosts_count);

// Deprecated: use `boxlite_options_set_auto_delete_interval`.
void boxlite_options_set_auto_remove(CBoxliteOptions *opts, int val);

// Set how long an idle box may remain paused before the runtime pauses it.
//
// The value is expressed in seconds. `0` preserves the runtime/control-plane
// default. A null options pointer is treated as a no-op.
void boxlite_options_set_auto_stop_interval(CBoxliteOptions *opts, uint32_t seconds);

// Set how long a stopped box may remain before the runtime deletes it.
//
// The value is expressed in seconds. `0` disables automatic deletion for
// persistent boxes. A null options pointer is treated as a no-op.
void boxlite_options_set_auto_delete_interval(CBoxliteOptions *opts, uint32_t seconds);

// Configure whether stopped boxes may automatically resume on demand.
//
// Any non-zero `val` enables auto-resume; `0` disables it. A null options
// pointer is treated as a no-op.
void boxlite_options_set_auto_resume_enabled(CBoxliteOptions *opts, int val);

void boxlite_options_set_detach(CBoxliteOptions *opts, int val);

// Apply a `CAdvancedBoxOptions` (capabilities, security, mount isolation, health check) to a
// `CBoxliteOptions`. Clones the advanced configuration into the box options —
// the caller retains ownership of `advanced_opts` and is responsible for
// freeing it via `boxlite_advanced_options_free`.
//
// Either pointer being null is a no-op. Security is reached through the
// advanced layer, mirroring the core model (`BoxOptions.advanced.security`):
// build the `CAdvancedBoxOptions` handle via `boxlite_advanced_options_new`,
// toggle the sandbox with `boxlite_advanced_options_set_security_enabled`,
// then apply it here.
void boxlite_options_set_advanced(CBoxliteOptions *opts, const CAdvancedBoxOptions *advanced_opts);

void boxlite_options_set_entrypoint(CBoxliteOptions *opts, const char *const *args, int argc);

void boxlite_options_set_cmd(CBoxliteOptions *opts, const char *const *args, int argc);

void boxlite_options_free(CBoxliteOptions *opts);

// Create an API-key credential.
//
// # Arguments
// - `key`: opaque API key sent as `Authorization: Bearer` (required).
// - `out_credential`: receives the credential handle on success.
// - `out_error`: receives error code + message on failure (nullable).
//
// Returns `BoxliteErrorCode::Ok` on success. Free the handle with
// `boxlite_credential_free`.
//
// # Safety
// `out_credential` must be non-NULL; `key` must be a valid C string.
enum BoxliteErrorCode boxlite_api_key_credential_new(const char *key,
                                                     CBoxliteCredential **out_credential,
                                                     CBoxliteError *out_error);

// Free a credential handle. No-op on NULL.
//
// # Safety
// `credential` must be a handle from `boxlite_api_key_credential_new`
// or NULL, and must not be used after this call.
void boxlite_credential_free(CBoxliteCredential *credential);

// Create REST options for `url` (no credential, server-default prefix).
//
// # Arguments
// - `url`: REST API base URL (required, e.g. `https://api.example.com`).
// - `out_options`: receives the options handle on success.
// - `out_error`: receives error code + message on failure (nullable).
//
// Returns `BoxliteErrorCode::Ok` on success. Free the handle with
// `boxlite_rest_options_free`.
//
// # Safety
// `out_options` must be non-NULL; `url` must be a valid C string.
enum BoxliteErrorCode boxlite_rest_options_new(const char *url,
                                               CBoxliteRestOptions **out_options,
                                               CBoxliteError *out_error);

// Attach a credential to the options. The credential's inner reference
// is cloned into the options, so the caller still owns `credential`
// and must free it independently with `boxlite_credential_free`.
// No-op if either pointer is NULL.
//
// # Safety
// `options` and `credential` must be valid handles or NULL.
void boxlite_rest_options_set_credential(CBoxliteRestOptions *options,
                                         const CBoxliteCredential *credential);

// Set the routing-slot value substituted into the `{prefix}`
// URL segment on box-scoped requests. Opaque — the server tells
// the client what to use here via `Principal.path_prefix` from
// `GET /v1/me`. No-op if `options` is NULL or `path_prefix` is
// not a valid C string. When unset, the client builds URLs
// without the segment (`/v1/boxes/...`) — the single-tenant
// deployment shape.
//
// # Safety
// `options` must be a valid handle or NULL; `path_prefix` a valid
// C string or NULL.
void boxlite_rest_options_set_path_prefix(CBoxliteRestOptions *options, const char *path_prefix);

// Free a REST options handle. No-op on NULL.
//
// # Safety
// `options` must be a handle from `boxlite_rest_options_new` or NULL,
// and must not be used after this call.
void boxlite_rest_options_free(CBoxliteRestOptions *options);

// Create a runtime that connects to a remote BoxLite REST server using
// the supplied options.
//
// # Arguments
// - `options`: a handle from `boxlite_rest_options_new` (required).
// - `out_runtime`: receives the runtime handle on success.
// - `out_error`: receives error code + message on failure (nullable).
//
// Returns `BoxliteErrorCode::Ok` on success. The runtime handle is
// freed with `boxlite_runtime_free`. `options` is unchanged and must
// still be freed by the caller with `boxlite_rest_options_free`.
//
// # Safety
// `options` and `out_runtime` must be non-NULL.
enum BoxliteErrorCode boxlite_rest_runtime_new_with_options(const CBoxliteRestOptions *options,
                                                            CBoxliteRuntime **out_runtime,
                                                            CBoxliteError *out_error);

const char *boxlite_version(void);

enum BoxliteErrorCode boxlite_runtime_new(const char *home_dir,
                                          const struct BoxliteImageRegistry *image_registries,
                                          int image_registries_count,
                                          CBoxliteRuntime **out_runtime,
                                          CBoxliteError *out_error);

enum BoxliteErrorCode boxlite_runtime_images(CBoxliteRuntime *runtime,
                                             CBoxliteImageHandle **out_handle,
                                             CBoxliteError *out_error);

// Create a runtime-scoped named-volume handle.
//
// On success ownership of `*out_handle` transfers to the caller, which must
// release it with `boxlite_volume_free`. The handle may submit operations from
// multiple threads; callbacks run only while the parent runtime is drained.
// `out_error` may be null and otherwise receives synchronous failures.
//
// # Safety
//
// `runtime` must be a live runtime pointer and `out_handle` must be non-null
// and writable. The returned handle must not be used after it is freed.
enum BoxliteErrorCode boxlite_runtime_volumes(CBoxliteRuntime *runtime,
                                              CBoxliteVolumeHandle **out_handle,
                                              CBoxliteError *out_error);

// Async + callback variant of runtime shutdown.
//
// Spawns a Tokio task that calls `BoxliteRuntime::shutdown` and posts a
// `RuntimeEvent::Shutdown` to the runtime queue. Marks liveness as closed
// synchronously so subsequent ops fail fast.
enum BoxliteErrorCode boxlite_runtime_shutdown(CBoxliteRuntime *runtime,
                                               int timeout_secs,
                                               CRuntimeShutdownCb cb,
                                               void *user_data,
                                               CBoxliteError *out_error);

void boxlite_runtime_free(CBoxliteRuntime *runtime);

// Drain pending callbacks for `runtime`, dispatching them on the calling
// thread. The queue lock is released before any user code runs.
//
// `timeout_ms`:
//   - `0`  : non-blocking poll
//   - `< 0`: block indefinitely until at least one event is available
//   - `> 0`: block up to that many milliseconds
//
// Returns the number of dispatched events, or `-1` on error.
int boxlite_runtime_drain(CBoxliteRuntime *runtime, int timeout_ms, CBoxliteError *out_error);

void boxlite_free_string(char *s);

// Queue asynchronous volume creation.
//
// `Ok` means queueing succeeded. The callback runs later on the thread calling
// `boxlite_runtime_drain`; successful metadata ownership transfers to it.
// Calls may be submitted concurrently. `user_data` is passed through unchanged
// and must remain usable by the caller until callback dispatch.
//
// # Safety
//
// `handle` and `cb` must be non-null. `out_error` may be null; otherwise it must
// be writable and receives synchronous queueing failures only. The handle must
// remain valid until this function returns. A successful callback must release
// its metadata with `boxlite_free_volume_info`; the error pointer is borrowed.
enum BoxliteErrorCode boxlite_volume_create(CBoxliteVolumeHandle *handle,
                                            const char *name,
                                            CBoxVolumeCreateCb cb,
                                            void *user_data,
                                            CBoxliteError *out_error);

// Queue asynchronous volume listing with the same dispatch, concurrency, and
// `user_data` contract as [`boxlite_volume_create`].
//
// # Safety
//
// `handle` and `cb` must be non-null; `out_error` may be null. A successful
// callback owns its list and must call `boxlite_free_volume_info_list`.
enum BoxliteErrorCode boxlite_volume_list(CBoxliteVolumeHandle *handle,
                                          CBoxVolumeListCb cb,
                                          void *user_data,
                                          CBoxliteError *out_error);

// Queue asynchronous lookup of a volume by id with the same dispatch,
// concurrency, and `user_data` contract as [`boxlite_volume_create`].
//
// # Safety
//
// `handle`, `id`, and `cb` must be non-null. `id` must contain UTF-8 and only
// needs to remain valid for this call. `out_error` may be null. A successful
// callback owns its metadata and must call `boxlite_free_volume_info`.
enum BoxliteErrorCode boxlite_volume_get(CBoxliteVolumeHandle *handle,
                                         const char *id,
                                         CBoxVolumeGetCb cb,
                                         void *user_data,
                                         CBoxliteError *out_error);

// Queue asynchronous removal of a volume by id with the same dispatch,
// concurrency, and `user_data` contract as [`boxlite_volume_create`]. A
// non-zero `force` requests success when the volume is absent.
//
// # Safety
//
// `handle`, `id`, and `cb` must be non-null. `id` must contain UTF-8 and only
// needs to remain valid for this call. `out_error` may be null. Callback
// arguments are borrowed for dispatch and require no result deallocation.
enum BoxliteErrorCode boxlite_volume_remove(CBoxliteVolumeHandle *handle,
                                            const char *id,
                                            int force,
                                            CBoxVolumeRemoveCb cb,
                                            void *user_data,
                                            CBoxliteError *out_error);

// Free a volume handle returned by `boxlite_runtime_volumes`.
//
// # Safety
//
// `handle` must be null or a pointer previously returned by
// `boxlite_runtime_volumes` that has not already been freed. Callers must not
// use the handle after this function returns.
void boxlite_volume_free(CBoxliteVolumeHandle *handle);

// Free a standalone `CVolumeInfo` and its owned strings.
//
// # Safety
//
// `info` must be null or a pointer allocated by this module that has not
// already been freed.
void boxlite_free_volume_info(struct CVolumeInfo *info);

// Free a `CVolumeInfoList`, all entries, and their owned strings.
//
// # Safety
//
// `list` must be null or a pointer allocated by this module that has not
// already been freed.
void boxlite_free_volume_info_list(struct CVolumeInfoList *list);

#ifdef __cplusplus
}  // extern "C"
#endif  // __cplusplus

#endif  /* BOXLITE_H */
