package boxlite

/*
#include <boxlite.h>
#include <stdlib.h>
*/
import "C"
import (
	"fmt"
	"os"
	"strings"
	"unsafe"
)

// RuntimeOption configures a Runtime.
type RuntimeOption func(*runtimeConfig)

type runtimeConfig struct {
	homeDir         string
	imageRegistries []ImageRegistry
}

// RegistryTransport selects the transport used to contact an OCI registry.
type RegistryTransport string

const (
	RegistryTransportHTTPS RegistryTransport = "https"
	RegistryTransportHTTP  RegistryTransport = "http"
)

// ImageRegistryAuth configures credentials for an OCI registry.
type ImageRegistryAuth struct {
	Username    string
	Password    string
	BearerToken string
}

// ImageRegistry configures an OCI registry host.
type ImageRegistry struct {
	Host       string
	Transport  RegistryTransport
	SkipVerify bool
	Search     bool
	Auth       ImageRegistryAuth
}

// WithHomeDir sets the BoxLite data directory.
func WithHomeDir(dir string) RuntimeOption {
	return func(c *runtimeConfig) { c.homeDir = dir }
}

// WithImageRegistry configures transport, TLS, search, and auth for a registry.
func WithImageRegistry(registry ImageRegistry) RuntimeOption {
	return func(c *runtimeConfig) { c.imageRegistries = append(c.imageRegistries, registry) }
}

// WithImageRegistries configures multiple image registries.
func WithImageRegistries(registries ...ImageRegistry) RuntimeOption {
	return func(c *runtimeConfig) { c.imageRegistries = append(c.imageRegistries, registries...) }
}

// BoxOption configures a Box.
type BoxOption func(*boxConfig)

type NetworkMode string

const (
	NetworkModeEnabled  NetworkMode = "enabled"
	NetworkModeDisabled NetworkMode = "disabled"
)

// OutboundNetworkSpec configures guest-initiated egress. A non-empty
// AllowNet restricts both TCP and UDP egress. Hostname entries are enforced
// by TLS SNI / HTTP Host inspection, which only TCP carries, so a
// hostname-only AllowNet denies all UDP egress — add the IP or CIDR to keep
// UDP open.
type OutboundNetworkSpec struct {
	Mode     NetworkMode
	AllowNet []string
}

// InboundNetworkSpec configures whether services running inside the box are
// reachable from outside it. Aligned field-for-field with
// OutboundNetworkSpec: ModeEnabled means publicly reachable, ModeDisabled
// means private. AllowNet exists for shape symmetry only — no layer
// enforces an inbound allowlist yet, so a non-empty value is rejected.
type InboundNetworkSpec struct {
	Mode     NetworkMode
	AllowNet []string
}

// NetworkSpec configures guest networking: Outbound covers guest-initiated
// egress, Inbound covers whether services the box exposes are reachable
// from outside it. The two are independent — set either, both, or neither.
type NetworkSpec struct {
	Outbound OutboundNetworkSpec
	Inbound  InboundNetworkSpec

	// Mode is the pre-split outbound mode, kept so existing callers keep
	// compiling and behaving the same.
	//
	// Deprecated: use Outbound.Mode. Setting both Mode and Outbound is
	// rejected when the options are converted rather than resolved silently.
	Mode NetworkMode

	// AllowNet is the pre-split outbound allowlist.
	//
	// Deprecated: use Outbound.AllowNet.
	AllowNet []string
}

// resolve folds the deprecated flat fields into Outbound, which is the
// direction they configured before the outbound/inbound split. Returns an
// error when a caller supplies both shapes.
func (s NetworkSpec) resolve() (NetworkSpec, error) {
	legacySet := s.Mode != "" || len(s.AllowNet) > 0
	nestedSet := s.Outbound.Mode != "" || len(s.Outbound.AllowNet) > 0
	if legacySet && nestedSet {
		return NetworkSpec{}, fmt.Errorf("network cannot mix Outbound with deprecated Mode/AllowNet")
	}
	if legacySet {
		s.Outbound = OutboundNetworkSpec{Mode: s.Mode, AllowNet: s.AllowNet}
	}
	s.Mode = ""
	s.AllowNet = nil
	return s, nil
}

// PortProtocol selects the transport protocol for a port forwarding rule.
type PortProtocol int

const (
	// PortProtocolUnknown is the unset port protocol value.
	PortProtocolUnknown PortProtocol = iota
	// PortProtocolTcp forwards TCP traffic.
	PortProtocolTcp
	// PortProtocolUdp represents UDP traffic. The boxlite runtime does not
	// support UDP forwarding yet, so PortSpec validation rejects it before FFI.
	PortProtocolUdp
)

func (p PortProtocol) String() string {
	switch p {
	case PortProtocolTcp:
		return "TCP"
	case PortProtocolUdp:
		return "UDP"
	default:
		return "Unknown"
	}
}

// PortSpec configures a host-to-guest port forwarding rule.
//
// Host is the host-side port number. Use 0 to let the OS select an available
// port; explicit host ports must be in 1..65535. Guest is the guest-side port
// number and must be in 1..65535. Protocol selects the transport protocol;
// PortProtocolUnknown defaults to TCP. The boxlite runtime currently forwards
// TCP only. HostIP defaults to all host interfaces when empty.
type PortSpec struct {
	Host     int
	Guest    int
	Protocol PortProtocol
	HostIP   string
}

type cPortSpec struct {
	host_port  int
	guest_port int
	protocol   PortProtocol
	host_ip    string
}

func (p PortSpec) toCSpec() (cPortSpec, error) {
	if p.Guest < 1 || p.Guest > 65535 {
		return cPortSpec{}, fmt.Errorf("guest port must be in range 1-65535, got %d", p.Guest)
	}
	if p.Host < 0 || p.Host > 65535 {
		return cPortSpec{}, fmt.Errorf("host port must be in range 0-65535, got %d", p.Host)
	}
	protocol := p.Protocol
	switch protocol {
	case PortProtocolUnknown:
		protocol = PortProtocolTcp
	case PortProtocolTcp:
	case PortProtocolUdp:
		return cPortSpec{}, fmt.Errorf("port protocol %s is not supported by the boxlite runtime yet", p.Protocol)
	default:
		return cPortSpec{}, fmt.Errorf("invalid port protocol %s", p.Protocol)
	}
	return cPortSpec{
		host_port:  p.Host,
		guest_port: p.Guest,
		protocol:   protocol,
		host_ip:    p.HostIP,
	}, nil
}

// cPortProtocol maps by explicit switch: the Go enum reserves 0 for the unset
// value while the C enum starts at Tcp = 0, so a numeric cast would be wrong.
// Returns plain uint32 because cgo maps C enum parameters to their underlying
// integer type (same shape as cRegistryTransport).
func cPortProtocol(p PortProtocol) uint32 {
	switch p {
	case PortProtocolUdp:
		return uint32(C.BoxlitePortProtocolUdp)
	default:
		return uint32(C.BoxlitePortProtocolTcp)
	}
}

// Secret configures outbound HTTPS secret substitution.
type Secret struct {
	Name        string
	Value       string
	Hosts       []string
	Placeholder string
}

type boxConfig struct {
	name       string
	cpus       int
	memoryMiB  int
	diskSizeGB int
	rootfsPath string
	env        [][2]string
	volumes    []volumeEntry
	ports      []PortSpec
	workDir    string
	user       string
	entrypoint []string
	cmd        []string
	autoRemove *bool
	autoStop   *uint32
	autoDelete *uint32
	autoResume *bool
	detach     *bool
	network    *NetworkSpec
	networkErr error // deferred WithNetwork validation error, surfaced at conversion
	secrets    []Secret
	advanced   *AdvancedBoxOptions // nil = runtime defaults; non-nil = caller-owned advanced opts applied via boxlite_options_set_advanced
}

// volumeEntry is one mount. Exactly one origin is set: managedVolume for a
// server-side managed volume, hostPath for a bind from the local filesystem.
type volumeEntry struct {
	managedVolume string
	hostPath      string
	guestPath     string
	readOnly      bool
}

// WithName sets a human-readable name for the box.
func WithName(name string) BoxOption {
	return func(c *boxConfig) { c.name = name }
}

// WithCPUs sets the number of virtual CPUs.
func WithCPUs(n int) BoxOption {
	return func(c *boxConfig) { c.cpus = n }
}

// WithMemory sets the memory limit in MiB.
func WithMemory(mib int) BoxOption {
	return func(c *boxConfig) { c.memoryMiB = mib }
}

// WithDiskSize sets the per-box COW disk virtual size in GB.
// When unset, the COW disk inherits the base ext4 image size, which is
// content-fitted (~256 MB minimum). Set this to give the sandbox runtime
// write headroom; the guest's ext4 is automatically resized via resize2fs
// on first boot.
func WithDiskSize(gb int) BoxOption {
	return func(c *boxConfig) { c.diskSizeGB = gb }
}

// WithRootfsPath prefers a local OCI image layout directory over pulling from a registry.
//
// If the path exists and is a directory, it is used and the image argument to
// [Runtime.Create] is ignored. Otherwise BoxLite falls back to the image reference
// (for example when the directory has not been exported yet).
//
// The directory should contain a valid OCI bundle (oci-layout, index.json, blobs/sha256/, …).
func WithRootfsPath(path string) BoxOption {
	return func(c *boxConfig) { c.rootfsPath = path }
}

// WithEnv adds an environment variable.
func WithEnv(key, value string) BoxOption {
	return func(c *boxConfig) {
		c.env = append(c.env, [2]string{key, value})
	}
}

// WithBindMount binds a host path into the box.
//
// Host binds are local-runtime only; a REST runtime rejects them at create.
// Use [WithManagedVolume] against a REST runtime.
func WithBindMount(hostPath, guestPath string) BoxOption {
	return func(c *boxConfig) {
		c.volumes = append(c.volumes, volumeEntry{hostPath: hostPath, guestPath: guestPath})
	}
}

// WithBindMountReadOnly binds a host path into the box as read-only.
func WithBindMountReadOnly(hostPath, guestPath string) BoxOption {
	return func(c *boxConfig) {
		c.volumes = append(c.volumes, volumeEntry{hostPath: hostPath, guestPath: guestPath, readOnly: true})
	}
}

// WithManagedVolume mounts a managed volume into the box.
//
// managedVolume is the volume's server-assigned id or its name — the server
// resolves either.
//
// A local runtime resolves it against its own volume store when the box is
// created; a REST runtime forwards it to the server as-is.
func WithManagedVolume(managedVolume, guestPath string) BoxOption {
	return func(c *boxConfig) {
		c.volumes = append(c.volumes, volumeEntry{managedVolume: managedVolume, guestPath: guestPath})
	}
}

// Read-only managed volumes have no WithManagedVolumeReadOnly counterpart:
// the server rejects read_only on a managed mount, so the option could only
// ever produce an error. Host binds keep WithBindMountReadOnly.

// WithPort publishes a guest port on a host port.
//
// The boxlite runtime currently forwards TCP only. Host 0 asks the OS to select
// an available local port.
func WithPort(spec PortSpec) BoxOption {
	return func(c *boxConfig) {
		c.ports = append(c.ports, spec)
	}
}

// WithWorkDir sets the working directory inside the container.
func WithWorkDir(dir string) BoxOption {
	return func(c *boxConfig) { c.workDir = dir }
}

// WithUser sets the user the container process runs as, in Docker's
// `<name|uid>[:<group|gid>]` form. Empty leaves the image's USER directive
// in force.
func WithUser(user string) BoxOption {
	return func(c *boxConfig) { c.user = user }
}

// WithEntrypoint overrides the image's ENTRYPOINT.
func WithEntrypoint(args ...string) BoxOption {
	return func(c *boxConfig) { c.entrypoint = args }
}

// WithCmd overrides the image's CMD.
func WithCmd(args ...string) BoxOption {
	return func(c *boxConfig) { c.cmd = args }
}

// WithNetwork sets the structured network configuration for the box.
func WithNetwork(spec NetworkSpec) BoxOption {
	return func(c *boxConfig) {
		resolved, err := spec.resolve()
		if err != nil {
			c.networkErr = err
			return
		}
		spec = resolved
		outboundAllowNet := append([]string(nil), spec.Outbound.AllowNet...)
		inboundAllowNet := append([]string(nil), spec.Inbound.AllowNet...)
		c.network = &NetworkSpec{
			Outbound: OutboundNetworkSpec{
				Mode:     spec.Outbound.Mode,
				AllowNet: outboundAllowNet,
			},
			Inbound: InboundNetworkSpec{
				Mode:     spec.Inbound.Mode,
				AllowNet: inboundAllowNet,
			},
		}
	}
}

// WithSecret adds an outbound HTTPS secret substitution rule.
func WithSecret(secret Secret) BoxOption {
	return func(c *boxConfig) {
		c.secrets = append(c.secrets, secret)
	}
}

// WithAutoStopInterval configures the cloud AutoStop idle TTL in seconds.
// A value of 0 disables AutoStop. Local runtimes return Unsupported.
func WithAutoStopInterval(seconds uint32) BoxOption {
	return func(c *boxConfig) { c.autoStop = &seconds }
}

// WithAutoDeleteInterval configures the cloud AutoDelete TTL in seconds.
// A value of 0 disables AutoDelete. Local runtimes return Unsupported.
func WithAutoDeleteInterval(seconds uint32) BoxOption {
	return func(c *boxConfig) { c.autoDelete = &seconds }
}

// WithAutoResumeEnabled configures whether the box automatically resumes when
// accessed after AutoStop. Defaults to true to preserve existing behavior.
func WithAutoResumeEnabled(enabled bool) BoxOption {
	return func(c *boxConfig) { c.autoResume = &enabled }
}

// WithAutoRemove sets whether the box is auto-removed on stop.
// Deprecated: use WithAutoDeleteInterval.
func WithAutoRemove(v bool) BoxOption {
	return func(c *boxConfig) { c.autoRemove = &v }
}

// WithDetach sets whether the box survives parent process exit.
func WithDetach(v bool) BoxOption {
	return func(c *boxConfig) { c.detach = &v }
}

// buildAndFreeCOptions runs buildCOptions, immediately frees the C
// handle on success, and returns just the error. Exists for unit
// tests in `_test.go` files, which Go forbids from using cgo
// directly — without this helper they can't exercise buildCOptions
// because the caller has to free `*C.CBoxliteOptions`.
func buildAndFreeCOptions(image string, cfg *boxConfig) error {
	opts, err := buildCOptions(image, cfg)
	if err != nil {
		return err
	}
	if opts != nil {
		C.boxlite_options_free(opts)
	}
	return nil
}

// WithAdvancedOptions attaches advanced capability and security options to the
// box, mirroring the core `BoxOptions.advanced` model.
//
// Build the handle via NewAdvancedBoxOptions and configure it with
// SetCapabilities and/or SetSecurityEnabled. The caller retains ownership and
// must call `adv.Close()` after the box has been created (or sooner, if
// discarded). If never called, the box uses the defaults.
//
//	adv, _ := boxlite.NewAdvancedBoxOptions()
//	defer adv.Close()
//	adv.SetSecurityEnabled(false) // opt out of the sandbox
//	box, _ := runtime.Create(ctx, "alpine:latest",
//	    boxlite.WithAdvancedOptions(adv))
func WithAdvancedOptions(adv *AdvancedBoxOptions) BoxOption {
	return func(c *boxConfig) { c.advanced = adv }
}

func buildCOptions(image string, cfg *boxConfig) (*C.CBoxliteOptions, error) {
	image = strings.TrimSpace(image)
	rootfsPath := strings.TrimSpace(cfg.rootfsPath)

	useLocalOCI := false
	if rootfsPath != "" {
		if fi, err := os.Stat(rootfsPath); err == nil && fi.IsDir() {
			useLocalOCI = true
		}
	}
	if image == "" && !useLocalOCI {
		return nil, fmt.Errorf("boxlite: image reference is required when WithRootfsPath is unset, missing, or not a directory")
	}

	cImage := toCString(image)
	defer C.free(unsafe.Pointer(cImage))

	var cOpts *C.CBoxliteOptions
	var cerr C.CBoxliteError
	code := C.boxlite_options_new(cImage, &cOpts, &cerr)
	if code != C.Ok {
		return nil, freeError(&cerr)
	}

	if useLocalOCI {
		cPath := toCString(rootfsPath)
		C.boxlite_options_set_rootfs_path(cOpts, cPath)
		C.free(unsafe.Pointer(cPath))
	}
	if cfg.name != "" {
		cName := toCString(cfg.name)
		C.boxlite_options_set_name(cOpts, cName)
		C.free(unsafe.Pointer(cName))
	}
	if cfg.cpus > 0 {
		C.boxlite_options_set_cpus(cOpts, C.int(cfg.cpus))
	}
	if cfg.memoryMiB > 0 {
		C.boxlite_options_set_memory(cOpts, C.int(cfg.memoryMiB))
	}
	if cfg.diskSizeGB > 0 {
		C.boxlite_options_set_disk_size_gb(cOpts, C.int(cfg.diskSizeGB))
	}
	if cfg.workDir != "" {
		cDir := toCString(cfg.workDir)
		C.boxlite_options_set_workdir(cOpts, cDir)
		C.free(unsafe.Pointer(cDir))
	}
	if cfg.user != "" {
		cUser := toCString(cfg.user)
		C.boxlite_options_set_user(cOpts, cUser)
		C.free(unsafe.Pointer(cUser))
	}
	for _, env := range cfg.env {
		cKey := toCString(env[0])
		cValue := toCString(env[1])
		C.boxlite_options_add_env(cOpts, cKey, cValue)
		C.free(unsafe.Pointer(cKey))
		C.free(unsafe.Pointer(cValue))
	}
	for _, volume := range cfg.volumes {
		cGuest := toCString(volume.guestPath)
		readOnly := C.int(0)
		if volume.readOnly {
			readOnly = 1
		}
		if volume.managedVolume != "" {
			cVolume := toCString(volume.managedVolume)
			C.boxlite_options_add_managed_volume(cOpts, cVolume, cGuest, readOnly)
			C.free(unsafe.Pointer(cVolume))
		} else {
			cHost := toCString(volume.hostPath)
			C.boxlite_options_add_bind_mount(cOpts, cHost, cGuest, readOnly)
			C.free(unsafe.Pointer(cHost))
		}
		C.free(unsafe.Pointer(cGuest))
	}
	for _, port := range cfg.ports {
		cPort, err := port.toCSpec()
		if err != nil {
			C.boxlite_options_free(cOpts)
			return nil, err
		}
		var cHostIP *C.char
		if cPort.host_ip != "" {
			cHostIP = toCString(cPort.host_ip)
		}
		code := C.boxlite_options_add_port(
			cOpts,
			C.uint16_t(cPort.host_port),
			C.uint16_t(cPort.guest_port),
			cPortProtocol(cPort.protocol),
			cHostIP,
		)
		if cHostIP != nil {
			C.free(unsafe.Pointer(cHostIP))
		}
		if code != C.Ok {
			C.boxlite_options_free(cOpts)
			return nil, fmt.Errorf("add port %d:%d failed with code %d", cPort.host_port, cPort.guest_port, int(code))
		}
	}
	if cfg.networkErr != nil {
		C.boxlite_options_free(cOpts)
		return nil, cfg.networkErr
	}
	if cfg.network != nil {
		switch cfg.network.Outbound.Mode {
		case "", NetworkModeEnabled:
			C.boxlite_options_set_network_enabled(cOpts)
			for _, host := range cfg.network.Outbound.AllowNet {
				cHost := toCString(host)
				C.boxlite_options_add_network_allow(cOpts, cHost)
				C.free(unsafe.Pointer(cHost))
			}
		case NetworkModeDisabled:
			if len(cfg.network.Outbound.AllowNet) > 0 {
				C.boxlite_options_free(cOpts)
				return nil, fmt.Errorf("network.outbound.mode=%q is incompatible with allow_net", NetworkModeDisabled)
			}
			C.boxlite_options_set_network_disabled(cOpts)
		default:
			C.boxlite_options_free(cOpts)
			return nil, fmt.Errorf("invalid outbound network mode %q", cfg.network.Outbound.Mode)
		}
		// The field exists for shape symmetry with Outbound, but no layer
		// enforces an inbound allowlist yet — reject rather than accept a
		// restriction that silently doesn't apply.
		if len(cfg.network.Inbound.AllowNet) > 0 {
			C.boxlite_options_free(cOpts)
			return nil, fmt.Errorf("inbound.allow_net is not supported yet; remove it (inbound access is controlled by mode only)")
		}
		switch cfg.network.Inbound.Mode {
		case "", NetworkModeEnabled:
			C.boxlite_options_set_network_inbound_enabled(cOpts)
		case NetworkModeDisabled:
			C.boxlite_options_set_network_inbound_disabled(cOpts)
		default:
			C.boxlite_options_free(cOpts)
			return nil, fmt.Errorf("invalid inbound network mode %q", cfg.network.Inbound.Mode)
		}
	}
	for _, secret := range cfg.secrets {
		cName := toCString(secret.Name)
		cValue := toCString(secret.Value)
		placeholder := secret.Placeholder
		if placeholder == "" {
			placeholder = "<BOXLITE_SECRET:" + secret.Name + ">"
		}
		cPlaceholder := toCString(placeholder)
		cHosts, hostCount := toCStringArray(secret.Hosts)
		C.boxlite_options_add_secret(cOpts, cName, cValue, cPlaceholder, cHosts, C.int(hostCount))
		freeCStringArray(cHosts, hostCount)
		C.free(unsafe.Pointer(cName))
		C.free(unsafe.Pointer(cValue))
		C.free(unsafe.Pointer(cPlaceholder))
	}
	if cfg.autoStop != nil {
		C.boxlite_options_set_auto_stop_interval(cOpts, C.uint32_t(*cfg.autoStop))
	}
	if cfg.autoRemove != nil {
		C.boxlite_options_set_auto_remove(cOpts, boolToCInt(*cfg.autoRemove))
	}
	if cfg.autoDelete != nil {
		C.boxlite_options_set_auto_delete_interval(cOpts, C.uint32_t(*cfg.autoDelete))
	}
	if cfg.autoResume != nil {
		C.boxlite_options_set_auto_resume_enabled(cOpts, boolToCInt(*cfg.autoResume))
	}
	if cfg.detach != nil {
		C.boxlite_options_set_detach(cOpts, boolToCInt(*cfg.detach))
	}
	if cfg.advanced != nil && cfg.advanced.handle != nil {
		// Clone the caller-owned advanced options onto the box.
		// The Go-side handle stays caller-owned; the box has its own copy after
		// set_advanced returns.
		C.boxlite_options_set_advanced(cOpts, cfg.advanced.handle)
	}
	if cfg.entrypoint != nil {
		cArgs, argc := toCStringArray(cfg.entrypoint)
		C.boxlite_options_set_entrypoint(cOpts, cArgs, C.int(argc))
		freeCStringArray(cArgs, argc)
	}
	if cfg.cmd != nil {
		cArgs, argc := toCStringArray(cfg.cmd)
		C.boxlite_options_set_cmd(cOpts, cArgs, C.int(argc))
		freeCStringArray(cArgs, argc)
	}
	return cOpts, nil
}

func validateCapabilities(option string, capabilities []string) error {
	for _, capability := range capabilities {
		normalized := strings.ToUpper(capability)
		if normalized == "ALL" {
			continue
		}
		name := strings.TrimPrefix(normalized, "CAP_")
		if name == "" {
			return &Error{
				Code:    ErrInvalidArgument,
				Message: fmt.Sprintf("empty Linux capability in %s", option),
			}
		}
		for index, character := range []byte(name) {
			isLetter := character >= 'A' && character <= 'Z'
			isTail := isLetter || character >= '0' && character <= '9' || character == '_'
			if index == 0 && !isLetter || index > 0 && !isTail {
				return &Error{
					Code:    ErrInvalidArgument,
					Message: fmt.Sprintf("malformed Linux capability in %s: %q", option, capability),
				}
			}
		}
	}
	return nil
}

func boolToCInt(v bool) C.int {
	if v {
		return 1
	}
	return 0
}
