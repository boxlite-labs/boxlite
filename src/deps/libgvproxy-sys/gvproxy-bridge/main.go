package main

/*
#include <stdlib.h>

typedef void (*log_callback_fn)(int level, const char* message);

static void call_rust_log_callback(void* callback, int level, const char* msg) {
	if (callback != NULL) {
		((log_callback_fn)callback)(level, msg);
	}
}
*/
import "C"
import (
	"context"
	"encoding/json"
	"fmt"
	"io"
	"log"
	"net"
	"net/http"
	"os"
	"runtime"
	"runtime/debug"
	"sync"
	"time"
	"unsafe"

	"github.com/containers/gvisor-tap-vsock/pkg/transport"
	"github.com/containers/gvisor-tap-vsock/pkg/types"
	"github.com/containers/gvisor-tap-vsock/pkg/virtualnetwork"
	logrus "github.com/sirupsen/logrus"
)

// Log level constants (match Rust tracing)
const (
	LogLevelTrace = 0
	LogLevelDebug = 1
	LogLevelInfo  = 2
	LogLevelWarn  = 3
	LogLevelError = 4
)

// RustTracingLogrusHook forwards logrus logs directly to Rust tracing
type RustTracingLogrusHook struct{}

func (h *RustTracingLogrusHook) Levels() []logrus.Level {
	return logrus.AllLevels
}

func (h *RustTracingLogrusHook) Fire(entry *logrus.Entry) error {
	callbackMu.RLock()
	callback := rustLogCallback
	callbackMu.RUnlock()

	if callback == nil {
		return nil // No callback registered, skip
	}

	// Build message with fields
	buf := make([]byte, 0, 256)
	buf = append(buf, entry.Message...)

	// Add logrus fields as key=value pairs
	for k, v := range entry.Data {
		buf = append(buf, ' ')
		buf = append(buf, k...)
		buf = append(buf, '=')
		buf = append(buf, fmt.Sprint(v)...)
	}

	// Map logrus level to Rust level
	var rustLevel int
	switch entry.Level {
	case logrus.TraceLevel:
		rustLevel = LogLevelTrace
	case logrus.DebugLevel:
		rustLevel = LogLevelDebug
	case logrus.InfoLevel:
		rustLevel = LogLevelInfo
	case logrus.WarnLevel:
		rustLevel = LogLevelWarn
	case logrus.ErrorLevel, logrus.FatalLevel, logrus.PanicLevel:
		rustLevel = LogLevelError
	default:
		rustLevel = LogLevelInfo
	}

	// Call Rust callback
	cMsg := C.CString(string(buf))
	C.call_rust_log_callback(callback, C.int(rustLevel), cMsg)
	C.free(unsafe.Pointer(cMsg))

	return nil
}

// RustTracingWriter redirects standard log package output to Rust tracing
type RustTracingWriter struct{}

func (w *RustTracingWriter) Write(p []byte) (n int, err error) {
	callbackMu.RLock()
	callback := rustLogCallback
	callbackMu.RUnlock()

	if callback == nil {
		return len(p), nil // No callback registered, discard
	}

	// Standard log package messages are typically info level
	// Remove trailing newline if present
	msg := string(p)
	if len(msg) > 0 && msg[len(msg)-1] == '\n' {
		msg = msg[:len(msg)-1]
	}

	// Call Rust callback with info level
	cMsg := C.CString(msg)
	C.call_rust_log_callback(callback, C.int(LogLevelInfo), cMsg)
	C.free(unsafe.Pointer(cMsg))

	return len(p), nil
}

// Global callback management
var (
	rustLogCallback unsafe.Pointer
	callbackMu      sync.RWMutex
)

//export gvproxy_set_log_callback
func gvproxy_set_log_callback(callback unsafe.Pointer) {
	callbackMu.Lock()
	rustLogCallback = callback
	callbackMu.Unlock()

	if callback != nil {
		// Forward all logrus logs to Rust tracing
		logrus.SetLevel(logrus.TraceLevel) // Enable trace level to support RUST_LOG=gvproxy=trace
		logrus.SetFormatter(&logrus.TextFormatter{
			DisableTimestamp: true, // Rust tracing adds its own timestamp
			DisableColors:    true,
		})
		logrus.SetOutput(io.Discard) // Discard direct output, only use hook to forward to Rust
		logrus.AddHook(&RustTracingLogrusHook{})

		// Redirect standard log package to Rust tracing (for vendored code like tcpproxy)
		log.SetOutput(&RustTracingWriter{})
		log.SetFlags(0) // Rust tracing adds its own timestamp and prefix
	} else {
		// Reset logrus to default
		logrus.SetLevel(logrus.InfoLevel)
		logrus.SetFormatter(&logrus.TextFormatter{})
		logrus.SetOutput(os.Stderr)

		// Reset standard log package
		log.SetOutput(os.Stderr)
		log.SetFlags(log.LstdFlags)
	}
}

// DNSRecord represents an exact A record within a local DNS zone.
type DNSRecord struct {
	Name string `json:"name"`
	IP   string `json:"ip"`
}

// DNSZone represents a local DNS zone configuration
// These are local DNS records served by the gateway's embedded DNS server.
// Queries not matching any zone are forwarded to the host's system DNS.
type DNSZone struct {
	Name      string      `json:"name"`              // Zone name (e.g., "myapp.local.", "." for root)
	Records   []DNSRecord `json:"records,omitempty"` // Exact A records within the zone
	DefaultIP string      `json:"default_ip"`        // Default IP for unmatched queries in this zone
}

// GvproxyConfig matches the Rust structure (must stay in sync!)
type GvproxyConfig struct {
	SocketPath       string         `json:"socket_path"`
	Subnet           string         `json:"subnet"`
	GatewayIP        string         `json:"gateway_ip"`
	GatewayMac       string         `json:"gateway_mac"`
	GuestIP          string         `json:"guest_ip"`
	HostIP           string         `json:"host_ip"`
	GuestMac         string         `json:"guest_mac"`
	MTU              uint16         `json:"mtu"`
	DNSZones         []DNSZone      `json:"dns_zones"`
	DNSSearchDomains []string       `json:"dns_search_domains"`
	Debug            bool           `json:"debug"`
	CaptureFile      *string        `json:"capture_file,omitempty"`
	AllowNet         []string       `json:"allow_net,omitempty"`
	Secrets          []SecretConfig `json:"secrets,omitempty"`
	CACertPEM        string         `json:"ca_cert_pem,omitempty"`
	CAKeyPEM         string         `json:"ca_key_pem,omitempty"`
	// ControlSocketPath, when set, binds gvproxy's ServicesMux (dynamic port
	// forwarding / DNS / DHCP leases / stats / cam) to a host unix socket the
	// boxlite core dials. Empty => the services API is not exposed.
	ControlSocketPath string `json:"control_socket_path,omitempty"`

	// RateLimit, when set, shapes the guest link in both directions. Absent or
	// zero-sized in a direction means that direction is unlimited. See shaper.go.
	RateLimit *RateLimitConfig `json:"rate_limit,omitempty"`
}

// GvproxyInstance tracks a running gvisor-tap-vsock instance
type GvproxyInstance struct {
	ID            int64
	SocketPath    string
	Config        *types.Configuration
	Cancel        context.CancelFunc
	conn          net.Conn                       // For macOS UnixDgram (VFKit)
	listener      net.Listener                   // For Linux UnixStream (Qemu)
	vn            *virtualnetwork.VirtualNetwork // Virtual network for stats collection
	vnMu          sync.RWMutex                   // Protects vn field
	ca            *BoxCA                         // Ephemeral MITM CA (nil if no secrets)
	secretMatcher *SecretHostMatcher             // Hostname→secrets lookup (nil if no secrets)
}

func buildDNSZones(config GvproxyConfig, allowNetZones []types.Zone) []types.Zone {
	dnsZones := make([]types.Zone, 0, len(config.DNSZones)+1)
	for _, zone := range config.DNSZones {
		dnsZone := types.Zone{
			Name:      zone.Name,
			DefaultIP: net.ParseIP(zone.DefaultIP),
		}
		for _, record := range zone.Records {
			dnsZone.Records = append(dnsZone.Records, types.Record{
				Name: record.Name,
				IP:   net.ParseIP(record.IP),
			})
		}
		dnsZones = append(dnsZones, dnsZone)
	}

	if len(config.AllowNet) > 0 {
		dnsZones = append(dnsZones, allowNetZones...)
		logrus.WithField("rules", len(config.AllowNet)).Info("Network allowlist enabled (DNS sinkhole)")
	}

	return dnsZones
}

// newAllowNetFilter builds the egress allowlist for a box. It is the single
// place that decides which addresses are exempt from allow_net, so the test
// harness can build its filter the same way and a change here cannot pass
// unnoticed.
//
// The gateway and guest addresses are the virtual network's own endpoints
// rather than egress destinations. config.HostIP is deliberately not among
// them: it NATs to the host's loopback (buildTapConfig below), which makes it
// a real destination that must obey allow_net like any other. Its DNS record
// is still served unconditionally — resolving the alias is not egress.
//
// Returns nil when allow_net is empty, which the transport handlers read as
// "forward everything".
func newAllowNetFilter(config GvproxyConfig, exactIPs, suffixIPs map[string][]net.IP) *AllowNetFilter {
	f := NewAllowNetFilter(config.AllowNet, config.GatewayIP, config.GuestIP)
	if f != nil {
		f.SetResolvedHostIPs(exactIPs, suffixIPs)
	}
	return f
}

func buildTapConfig(config GvproxyConfig, protocol types.Protocol, allowNetZones []types.Zone) *types.Configuration {
	nat := make(map[string]string)
	gatewayVirtualIPs := []string{config.GatewayIP}
	if config.HostIP != "" {
		nat[config.HostIP] = "127.0.0.1"
		if config.HostIP != config.GatewayIP {
			gatewayVirtualIPs = append(gatewayVirtualIPs, config.HostIP)
		}
	}

	return &types.Configuration{
		Debug:             config.Debug,
		MTU:               int(config.MTU),
		Subnet:            config.Subnet,
		GatewayIP:         config.GatewayIP,
		GatewayMacAddress: config.GatewayMac,
		DHCPStaticLeases: map[string]string{
			config.GuestIP: config.GuestMac,
		},
		Forwards:          make(map[string]string),
		NAT:               nat,
		GatewayVirtualIPs: gatewayVirtualIPs,
		Protocol:          protocol,
		DNS:               buildDNSZones(config, allowNetZones),
		DNSSearchDomains:  config.DNSSearchDomains,
		CaptureFile:       "",
	}
}

var (
	instances   = make(map[int64]*GvproxyInstance)
	instancesMu sync.RWMutex
	nextID      int64 = 1
)

// On failure (return -1), the underlying error message is written to `*errOut`
// as a heap-allocated C string. Caller must free it via gvproxy_free_string.
// `errOut` may be nil if the caller doesn't want the message.
//
//export gvproxy_create
func gvproxy_create(configJSON *C.char, errOut **C.char) C.longlong {
	// setErr surfaces the underlying startup error back to the FFI caller so
	// the Rust runtime can include it in the user-visible BoxliteError instead
	// of reporting only an opaque "gvproxy_create failed".
	setErr := func(err error) {
		if errOut != nil {
			*errOut = C.CString(err.Error())
		}
	}

	goJSON := C.GoString(configJSON)

	var config GvproxyConfig
	if err := json.Unmarshal([]byte(goJSON), &config); err != nil {
		logrus.WithError(err).Error("Failed to parse gvproxy config")
		setErr(err)
		return -1
	}

	// Reject a bad rate limit outright rather than letting part of it apply.
	if err := config.RateLimit.Validate(); err != nil {
		logrus.WithError(err).Error("Invalid gvproxy rate limit config")
		setErr(err)
		return -1
	}

	instancesMu.Lock()
	id := nextID
	nextID++
	instancesMu.Unlock()

	// Use caller-provided socket path (unique per box)
	socketPath := config.SocketPath
	if socketPath == "" {
		logrus.Error("socket_path is required in GvproxyConfig")
		setErr(fmt.Errorf("socket_path is required in GvproxyConfig"))
		return -1
	}

	// Remove stale socket from a previous crash (safe: path is unique per box)
	if err := os.Remove(socketPath); err != nil && !os.IsNotExist(err) {
		logrus.WithFields(logrus.Fields{"error": err, "path": socketPath}).Warn("Failed to remove existing socket")
	}

	// Platform-specific protocol selection
	var protocol types.Protocol
	if runtime.GOOS == "darwin" {
		protocol = types.VfkitProtocol
	} else {
		protocol = types.QemuProtocol
	}

	// Resolve hostname rules once so the gateway DNS zones and the TCP egress
	// pin share the same IP set (see allow_net_filter.AllowHostToIP). This
	// resolution is frozen for the box's lifetime: a domain that changes IP
	// after startup is unreachable until the box is recreated. If re-resolution
	// is ever added, it must refresh the pin from the same source.
	// Skipped for an empty allow_net (the common case): building it would only
	// allocate a root sinkhole zone and log "DNS sinkhole configured" for a box
	// with no egress policy. buildDNSZones and newAllowNetFilter both already
	// gate on len(config.AllowNet) > 0, so a nil resolution is safe to pass.
	var resolved allowNetResolution
	if len(config.AllowNet) > 0 {
		resolved = buildAllowNet(config.AllowNet)
	}

	// Create gvisor-tap-vsock configuration from provided config
	tapConfig := buildTapConfig(config, protocol, resolved.zones)

	// Set CaptureFile if provided
	if config.CaptureFile != nil && *config.CaptureFile != "" {
		tapConfig.CaptureFile = *config.CaptureFile
		logrus.WithField("capture_file", *config.CaptureFile).Info("Packet capture enabled")
	}

	// Platform-specific socket creation
	var conn net.Conn
	var listener net.Listener
	var err error

	if runtime.GOOS == "darwin" {
		// macOS: Use UnixDgram with VFKit protocol (SOCK_DGRAM)
		socketURI := fmt.Sprintf("unixgram://%s", socketPath)
		conn, err = transport.ListenUnixgram(socketURI)
		if err != nil {
			logrus.WithFields(logrus.Fields{"error": err, "path": socketPath}).Error("Failed to create Unix datagram socket")
			setErr(fmt.Errorf("failed to create Unix datagram socket %q: %w", socketPath, err))
			return -1
		}
		logrus.WithField("path", socketPath).Info("Created UnixDgram socket for VFKit protocol")
	} else {
		// Linux: Use UnixStream with Qemu protocol (SOCK_STREAM)
		listener, err = net.Listen("unix", socketPath)
		if err != nil {
			logrus.WithFields(logrus.Fields{"error": err, "path": socketPath}).Error("Failed to create Unix stream socket")
			setErr(fmt.Errorf("failed to create Unix stream socket %q: %w", socketPath, err))
			return -1
		}
		logrus.WithField("path", socketPath).Info("Created UnixStream socket for Qemu protocol")
	}

	// Start gvisor-tap-vsock in background
	ctx, cancel := context.WithCancel(context.Background())

	instance := &GvproxyInstance{
		ID:         id,
		SocketPath: socketPath,
		Config:     tapConfig,
		Cancel:     cancel,
		conn:       conn,
		listener:   listener,
	}

	// Parse MITM CA from config (generated by Rust) when secrets are configured
	if config.CACertPEM != "" && config.CAKeyPEM != "" {
		ca, err := NewBoxCAFromPEM([]byte(config.CACertPEM), []byte(config.CAKeyPEM))
		if err != nil {
			logrus.WithError(err).Error("MITM: failed to parse CA from config")
			setErr(fmt.Errorf("MITM: failed to parse CA from config: %w", err))
			cancel()
			return -1
		}
		instance.ca = ca
		instance.secretMatcher = NewSecretHostMatcher(config.Secrets)
		logrus.WithField("num_secrets", len(config.Secrets)).Info("MITM: loaded CA from Rust config")
	}

	instancesMu.Lock()
	instances[id] = instance
	instancesMu.Unlock()

	// initErr keeps gvproxy_create synchronous until the virtual network and
	// optional ServicesMux control socket are ready, and surfaces startup
	// failures to the FFI caller.
	initErr := make(chan error, 1)

	// Start runtime metrics monitoring goroutine
	go func() {
		ticker := time.NewTicker(30 * time.Second)
		defer ticker.Stop()

		for {
			select {
			case <-ctx.Done():
				return
			case <-ticker.C:
				var memStats runtime.MemStats
				runtime.ReadMemStats(&memStats)

				logrus.WithFields(logrus.Fields{
					"id":            id,
					"goroutines":    runtime.NumGoroutine(),
					"os_threads":    runtime.GOMAXPROCS(0),
					"cgo_calls":     runtime.NumCgoCall(),
					"heap_alloc_mb": memStats.Alloc / 1024 / 1024,
					"sys_mb":        memStats.Sys / 1024 / 1024,
					"num_gc":        memStats.NumGC,
				}).Info("gvproxy runtime metrics")
			}
		}
	}()

	// Start virtual network in goroutine
	go func() {
		vn, err := virtualnetwork.New(tapConfig)
		if err != nil {
			logrus.WithFields(logrus.Fields{"error": err, "id": id}).Error("Failed to create virtual network")
			initErr <- err
			return
		}

		// Override the TCP and UDP handlers with the AllowNet filter and/or
		// MITM secret substitution
		if len(config.AllowNet) > 0 || instance.secretMatcher != nil {
			var allowNetFilter *AllowNetFilter
			if len(config.AllowNet) > 0 {
				allowNetFilter = newAllowNetFilter(config, resolved.exactIPs, resolved.suffixIPs)
			}
			// Fatal on purpose: the handlers left behind are upstream's
			// unfiltered forwarders, so a box that starts anyway would carry
			// an allow_net the caller believes in and the network ignores.
			if err := installAllowNetHandlers(vn, tapConfig, tapConfig.Ec2MetadataAccess, allowNetFilter, instance.ca, instance.secretMatcher); err != nil {
				logrus.WithFields(logrus.Fields{"error": err, "id": id}).Error("allowNet: failed to install transport handlers")
				initErr <- fmt.Errorf("failed to install allow_net transport handlers: %w", err)
				return
			}
		}

		// Store VirtualNetwork reference for stats collection
		instance.vnMu.Lock()
		instance.vn = vn
		instance.vnMu.Unlock()

		// Bind gvproxy's ServicesMux to a host unix socket so the boxlite core
		// can drive dynamic port forwarding / DNS / leases on the running box.
		// ServicesMux (not Mux) excludes the raw L2 /connect, so the VM's NIC
		// can never be attached through this socket.
		var controlListener net.Listener
		if config.ControlSocketPath != "" {
			// Remove a stale socket from a previous crash (path is unique per box).
			if rmErr := os.Remove(config.ControlSocketPath); rmErr != nil && !os.IsNotExist(rmErr) {
				logrus.WithFields(logrus.Fields{"error": rmErr, "path": config.ControlSocketPath}).Warn("Failed to remove existing services socket")
			}
			l, lErr := net.Listen("unix", config.ControlSocketPath)
			if lErr != nil {
				logrus.WithFields(logrus.Fields{"error": lErr, "path": config.ControlSocketPath}).Error("Failed to bind gvproxy services socket")
				initErr <- fmt.Errorf("failed to bind gvproxy services socket %q: %w", config.ControlSocketPath, lErr)
				return
			} else {
				controlListener = l
				logrus.WithField("path", config.ControlSocketPath).Info("Serving gvproxy ServicesMux")
				go func() {
					if sErr := http.Serve(l, vn.ServicesMux()); sErr != nil && ctx.Err() == nil {
						logrus.WithError(sErr).Error("gvproxy services HTTP server exited")
					}
				}()
			}
		}
		initErr <- nil

		// Platform-specific packet handling
		if runtime.GOOS == "darwin" {
			// macOS: Handle VFKit datagram packets
			// VFKit requires a two-step process:
			// 1. transport.AcceptVfkit() - Waits for incoming data and wraps listener with remote address
			// 2. vn.AcceptVfkit() - Handles the VFKit protocol
			go func() {
				logrus.WithField("id", id).Trace("Waiting for VFKit connection on UnixDgram socket")

				// Wait for incoming connection and get wrapped connection with remote address
				// AcceptVfkit peeks at the first packet to get the remote address
				wrappedConn, err := transport.AcceptVfkit(conn.(*net.UnixConn))
				if err != nil {
					logrus.WithFields(logrus.Fields{"error": err, "id": id}).Error("Failed to accept VFKit connection")
					return
				}

				logrus.WithFields(logrus.Fields{"id": id, "remote": wrappedConn.RemoteAddr().String()}).Info("VFKit connection accepted")

				// Handle the VFKit protocol with the wrapped connection
				if err := vn.AcceptVfkit(ctx, wrapConn(wrappedConn, 0, config.RateLimit)); err != nil {
					if ctx.Err() == nil {
						logrus.WithFields(logrus.Fields{"error": err, "id": id}).Error("AcceptVfkit error")
					}
				}
			}()
		} else {
			// Linux: Handle Qemu stream connections
			go func() {
				logrus.WithField("id", id).Trace("Waiting for Qemu connection on UnixStream socket")

				// Accept incoming connection (blocks until VM connects)
				acceptedConn, err := listener.Accept()
				if err != nil {
					if ctx.Err() == nil {
						logrus.WithFields(logrus.Fields{"error": err, "id": id}).Error("Failed to accept connection")
					}
					return
				}

				logrus.WithFields(logrus.Fields{"id": id, "remote": acceptedConn.RemoteAddr().String()}).Info("Qemu connection accepted")

				// Close listener after first connection (one VM per gvproxy instance)
				listener.Close()

				// Handle the Qemu protocol
				if err := vn.AcceptQemu(ctx, wrapConn(acceptedConn, 4, config.RateLimit)); err != nil {
					if ctx.Err() == nil {
						logrus.WithFields(logrus.Fields{"error": err, "id": id}).Error("AcceptQemu error")
					}
				}
			}()
		}

		// Wait for context cancellation
		<-ctx.Done()

		// Cleanup
		if controlListener != nil {
			// Closing the listener unblocks the http.Serve goroutine.
			controlListener.Close()
			os.Remove(config.ControlSocketPath)
		}
		if runtime.GOOS == "darwin" && conn != nil {
			conn.Close()
		} else if listener != nil {
			listener.Close()
		}
		os.Remove(socketPath)
	}()

	// Wait for virtualnetwork.New to complete before returning a valid id.
	// On failure, tear down the instance and surface -1 so the FFI caller
	// (Rust boxlite runtime) can fail fast with a clear error instead of
	// shipping a broken socket downstream.
	if err := <-initErr; err != nil {
		logrus.WithFields(logrus.Fields{"error": err, "id": id}).Error("gvproxy init failed; tearing down instance")
		setErr(err)
		cancel()
		instancesMu.Lock()
		delete(instances, id)
		instancesMu.Unlock()
		if runtime.GOOS == "darwin" && conn != nil {
			conn.Close()
		} else if listener != nil {
			listener.Close()
		}
		os.Remove(socketPath)
		return -1
	}

	logrus.Info("Created gvproxy instance", "id", id, "socket", socketPath, "protocol", protocol)
	return C.longlong(id)
}

//export gvproxy_free_string
func gvproxy_free_string(str *C.char) {
	C.free(unsafe.Pointer(str))
}

//export gvproxy_destroy
func gvproxy_destroy(id C.longlong) C.int {
	instancesMu.Lock()
	instance, ok := instances[int64(id)]
	if ok {
		delete(instances, int64(id))
	}
	instancesMu.Unlock()

	if !ok {
		return -1
	}

	// Cancel context to stop goroutines
	instance.Cancel()

	logrus.Info("Destroyed gvproxy instance", "id", id)
	return 0
}

//export gvproxy_get_stats
func gvproxy_get_stats(id C.longlong) *C.char {
	// Validate Early: Check instance exists
	instancesMu.RLock()
	instance, ok := instances[int64(id)]
	instancesMu.RUnlock()

	if !ok {
		return nil
	}

	// Validate Early: Check vn initialized
	// (instance.vn might not be set yet if called too early)
	instance.vnMu.RLock()
	vn := instance.vn
	instance.vnMu.RUnlock()

	if vn == nil {
		return nil
	}

	// Single Responsibility: Delegate to stats.go for collection
	stats := collectNetworkStats(vn)
	if stats == "" {
		return nil
	}

	// Explicit: CString allocates memory, caller must free it
	return C.CString(stats)
}

//export gvproxy_get_version
func gvproxy_get_version() *C.char {
	// Get gvisor-tap-vsock version from build info
	buildInfo, ok := debug.ReadBuildInfo()
	if !ok {
		return C.CString("unknown")
	}

	// Find gvisor-tap-vsock dependency
	for _, dep := range buildInfo.Deps {
		if dep.Path == "github.com/containers/gvisor-tap-vsock" {
			return C.CString(dep.Version)
		}
	}

	return C.CString("unknown")
}

func main() {
	// CGO library, no main needed
}
