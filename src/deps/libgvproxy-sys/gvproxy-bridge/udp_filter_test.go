package main

// udp_filter_test.go — allow_net must apply to UDP, not only TCP.
//
// These tests drive the real virtual network the way a guest does: raw
// Ethernet frames over the qemu stream protocol into vn.AcceptQemu. Nothing
// is stubbed — the packets traverse the same gVisor stack, the same NAT
// table, and the same transport handlers a running box uses.
//
// The unlisted destination is a TEST-NET address (198.51.100.9) that the
// config NAT-maps to loopback, so the forwarder's net.Dial lands on a
// test-owned listener instead of the internet. Policy is evaluated on the
// pre-NAT address (forked_tcp.go:83), so 198.51.100.9 is what the allowlist
// sees, exactly as it would for a real public IP.

import (
	"context"
	"encoding/binary"
	"errors"
	"fmt"
	"net"
	"strings"
	"testing"
	"time"

	"github.com/containers/gvisor-tap-vsock/pkg/types"
	"github.com/containers/gvisor-tap-vsock/pkg/virtualnetwork"
	"gvisor.dev/gvisor/pkg/tcpip"
	"gvisor.dev/gvisor/pkg/tcpip/checksum"
	"gvisor.dev/gvisor/pkg/tcpip/header"
	"gvisor.dev/gvisor/pkg/tcpip/transport/tcp"
	"gvisor.dev/gvisor/pkg/tcpip/transport/udp"
)

const (
	unlistedIP   = "198.51.100.9"
	allowedCIDR  = "198.51.100.1/32"
	guestSrcPort = 41234
	probePayload = "BOXLITE_UDP_ALLOW_NET_PROBE"
	// A forwarded datagram reaches loopback in microseconds; this only has to
	// outlast scheduler noise before we conclude the packet was dropped.
	forwardWindow = 2 * time.Second
)

// guestTap is the test's end of the virtual link: write frames to inject
// guest traffic, read frames the gateway sends back.
type guestTap struct {
	conn   net.Conn
	frames chan []byte
}

// startNetwork builds the same virtual network gvproxy_create builds
// (buildTapConfig → virtualnetwork.New → installAllowNetHandlers) and
// attaches a test-driven guest link to it.
func startNetwork(t *testing.T, allowNet []string) *guestTap {
	t.Helper()

	cfg := testGvproxyConfig()
	cfg.AllowNet = allowNet
	return startNetworkWith(t, cfg)
}

// startNetworkWith is startNetwork's body, taking the whole config so a test can
// set cfg.RateLimit and exercise the shaper on the same path gvproxy_create
// uses. Note net.Pipe is synchronous and unbuffered: it can show pacing and RX
// queueing, but it cannot show the TX socket-buffer backpressure chain, because
// there is no buffer to fill.
func startNetworkWith(t *testing.T, cfg GvproxyConfig) *guestTap {
	t.Helper()

	// Mirror gvproxy_create: only resolve hostname rules when an allow_net is
	// present. An empty allow_net is the common case and needs no resolution;
	// the nil zones/maps are safe because buildDNSZones and newAllowNetFilter
	// already gate on len(cfg.AllowNet) > 0.
	var resolved allowNetResolution
	if len(cfg.AllowNet) > 0 {
		resolved = buildAllowNet(cfg.AllowNet)
	}
	tapConfig := buildTapConfig(cfg, types.QemuProtocol, resolved.zones)
	// Route the unlisted TEST-NET destination to a test-owned loopback
	// listener. The forwarders dial the NAT-translated address; the allowlist
	// still sees 198.51.100.9.
	tapConfig.NAT[unlistedIP] = "127.0.0.1"

	vn, err := virtualnetwork.New(tapConfig)
	if err != nil {
		t.Fatalf("virtualnetwork.New: %v", err)
	}

	if len(cfg.AllowNet) > 0 {
		filter := newAllowNetFilter(cfg, resolved.exactIPs, resolved.suffixIPs)
		if err := installAllowNetHandlers(vn, tapConfig, tapConfig.Ec2MetadataAccess, filter, nil, nil); err != nil {
			t.Fatalf("installAllowNetHandlers: %v", err)
		}
	}

	guestSide, stackSide := net.Pipe()
	ctx, cancel := context.WithCancel(context.Background())
	go func() { _ = vn.AcceptQemu(ctx, wrapConn(stackSide, 4, cfg.RateLimit)) }()
	t.Cleanup(func() {
		cancel()
		_ = guestSide.Close()
	})

	tap := &guestTap{conn: guestSide, frames: make(chan []byte, 64)}
	// Drain the gateway→guest direction: the switch writes ARP requests and
	// replies synchronously over net.Pipe and would block without a reader.
	go tap.readLoop()
	return tap
}

func (g *guestTap) readLoop() {
	var size [4]byte
	for {
		if _, err := readFull(g.conn, size[:]); err != nil {
			return
		}
		frame := make([]byte, binary.BigEndian.Uint32(size[:]))
		if _, err := readFull(g.conn, frame); err != nil {
			return
		}
		if g.answerARP(frame) {
			continue
		}
		select {
		case g.frames <- frame:
		default: // capture buffer full: the tests only inspect DNS replies
		}
	}
}

// answerARP replies to the gateway's ARP request for the guest IP. Without
// it the stack has no link address for the guest and never delivers a reply
// packet, which a real guest's kernel would handle.
func (g *guestTap) answerARP(frame []byte) bool {
	if len(frame) < header.EthernetMinimumSize+header.ARPSize {
		return false
	}
	eth := header.Ethernet(frame)
	if eth.Type() != header.ARPProtocolNumber {
		return false
	}
	req := header.ARP(frame[header.EthernetMinimumSize:])
	if !req.IsValid() || req.Op() != header.ARPRequest {
		return false
	}
	cfg := testGvproxyConfig()
	guestIP := net.ParseIP(cfg.GuestIP).To4()
	if !net.IP(req.ProtocolAddressTarget()).Equal(guestIP) {
		return false
	}
	guestMAC, err := net.ParseMAC(cfg.GuestMac)
	if err != nil {
		return false
	}

	reply := make([]byte, header.EthernetMinimumSize+header.ARPSize)
	header.Ethernet(reply).Encode(&header.EthernetFields{
		SrcAddr: tcpip.LinkAddress(guestMAC),
		DstAddr: eth.SourceAddress(),
		Type:    header.ARPProtocolNumber,
	})
	arp := header.ARP(reply[header.EthernetMinimumSize:])
	arp.SetIPv4OverEthernet()
	arp.SetOp(header.ARPReply)
	copy(arp.HardwareAddressSender(), guestMAC)
	copy(arp.ProtocolAddressSender(), guestIP)
	copy(arp.HardwareAddressTarget(), req.HardwareAddressSender())
	copy(arp.ProtocolAddressTarget(), req.ProtocolAddressSender())

	var size [4]byte
	binary.BigEndian.PutUint32(size[:], uint32(len(reply)))
	_ = g.conn.SetWriteDeadline(time.Now().Add(5 * time.Second))
	_, _ = g.conn.Write(append(size[:], reply...))
	return true
}

// awaitUDPPayload returns the payload of the next UDP datagram the gateway
// sends to the guest from srcIP:srcPort.
func (g *guestTap) awaitUDPPayload(t *testing.T, srcIP string, srcPort uint16) []byte {
	t.Helper()
	deadline := time.After(forwardWindow)
	for {
		select {
		case frame := <-g.frames:
			payload, ok := parseUDPPayload(frame, srcIP, srcPort)
			if ok {
				return payload
			}
		case <-deadline:
			t.Fatalf("no UDP datagram from %s:%d reached the guest", srcIP, srcPort)
		}
	}
}

func parseUDPPayload(frame []byte, srcIP string, srcPort uint16) ([]byte, bool) {
	if len(frame) < header.EthernetMinimumSize+header.IPv4MinimumSize {
		return nil, false
	}
	if header.Ethernet(frame).Type() != header.IPv4ProtocolNumber {
		return nil, false
	}
	ip := header.IPv4(frame[header.EthernetMinimumSize:])
	if !ip.IsValid(len(ip)) || tcpip.TransportProtocolNumber(ip.Protocol()) != udp.ProtocolNumber {
		return nil, false
	}
	if net.IP(ip.SourceAddressSlice()).String() != srcIP {
		return nil, false
	}
	segment := header.UDP(ip.Payload())
	if len(segment) < header.UDPMinimumSize || segment.SourcePort() != srcPort {
		return nil, false
	}
	return segment.Payload(), true
}

func readFull(conn net.Conn, buf []byte) (int, error) {
	read := 0
	for read < len(buf) {
		n, err := conn.Read(buf[read:])
		read += n
		if err != nil {
			return read, err
		}
	}
	return read, nil
}

func (g *guestTap) send(t *testing.T, frame []byte) {
	t.Helper()
	var size [4]byte
	binary.BigEndian.PutUint32(size[:], uint32(len(frame)))
	if err := g.conn.SetWriteDeadline(time.Now().Add(5 * time.Second)); err != nil {
		t.Fatalf("SetWriteDeadline: %v", err)
	}
	if _, err := g.conn.Write(append(size[:], frame...)); err != nil {
		t.Fatalf("write frame: %v", err)
	}
}

// --- frame construction -----------------------------------------------------

func mac(t *testing.T, s string) tcpip.LinkAddress {
	t.Helper()
	parsed, err := net.ParseMAC(s)
	if err != nil {
		t.Fatalf("ParseMAC(%q): %v", s, err)
	}
	return tcpip.LinkAddress(parsed)
}

func addr(t *testing.T, s string) tcpip.Address {
	t.Helper()
	ip := net.ParseIP(s).To4()
	if ip == nil {
		t.Fatalf("not an IPv4 address: %q", s)
	}
	return tcpip.AddrFrom4Slice(ip)
}

// ipv4Frame wraps an already-checksummed transport segment in IPv4 +
// Ethernet, addressed to the gateway MAC so the switch delivers it to the
// stack (switch.go:281).
func ipv4Frame(t *testing.T, src, dst tcpip.Address, proto tcpip.TransportProtocolNumber, segment []byte) []byte {
	t.Helper()
	cfg := testGvproxyConfig()

	total := header.IPv4MinimumSize + len(segment)
	ip := header.IPv4(make([]byte, total))
	ip.Encode(&header.IPv4Fields{
		TotalLength: uint16(total),
		TTL:         64,
		Protocol:    uint8(proto),
		SrcAddr:     src,
		DstAddr:     dst,
	})
	ip.SetChecksum(^ip.CalculateChecksum())
	copy(ip[header.IPv4MinimumSize:], segment)

	frame := make([]byte, header.EthernetMinimumSize+total)
	header.Ethernet(frame).Encode(&header.EthernetFields{
		SrcAddr: mac(t, cfg.GuestMac),
		DstAddr: mac(t, cfg.GatewayMac),
		Type:    header.IPv4ProtocolNumber,
	})
	copy(frame[header.EthernetMinimumSize:], ip)
	return frame
}

func udpFrame(t *testing.T, dstIP string, dstPort uint16, payload []byte) []byte {
	t.Helper()
	cfg := testGvproxyConfig()
	src, dst := addr(t, cfg.GuestIP), addr(t, dstIP)

	length := header.UDPMinimumSize + len(payload)
	segment := header.UDP(make([]byte, length))
	segment.Encode(&header.UDPFields{
		SrcPort: guestSrcPort,
		DstPort: dstPort,
		Length:  uint16(length),
	})
	copy(segment.Payload(), payload)

	xsum := header.PseudoHeaderChecksum(udp.ProtocolNumber, src, dst, uint16(length))
	xsum = checksum.Checksum(payload, xsum)
	segment.SetChecksum(^segment.CalculateChecksum(xsum))

	return ipv4Frame(t, src, dst, udp.ProtocolNumber, segment)
}

func tcpSynFrame(t *testing.T, dstIP string, dstPort uint16) []byte {
	t.Helper()
	cfg := testGvproxyConfig()
	src, dst := addr(t, cfg.GuestIP), addr(t, dstIP)

	segment := header.TCP(make([]byte, header.TCPMinimumSize))
	segment.Encode(&header.TCPFields{
		SrcPort:    guestSrcPort,
		DstPort:    dstPort,
		SeqNum:     1000,
		DataOffset: header.TCPMinimumSize,
		Flags:      header.TCPFlagSyn,
		WindowSize: 65535,
	})
	xsum := header.PseudoHeaderChecksum(tcp.ProtocolNumber, src, dst, header.TCPMinimumSize)
	segment.SetChecksum(^segment.CalculateChecksum(xsum))

	return ipv4Frame(t, src, dst, tcp.ProtocolNumber, segment)
}

// --- minimal DNS wire format ------------------------------------------------
//
// Hand-rolled rather than pulled from miekg/dns, which is only an indirect
// dependency here.

func dnsQueryA(txnID uint16, name string) []byte {
	query := make([]byte, 12, 32)
	binary.BigEndian.PutUint16(query[0:], txnID)
	binary.BigEndian.PutUint16(query[2:], 0x0100) // standard query, recursion desired
	binary.BigEndian.PutUint16(query[4:], 1)      // QDCOUNT

	for _, label := range strings.Split(name, ".") {
		query = append(query, byte(len(label)))
		query = append(query, label...)
	}
	query = append(query, 0)          // root label
	query = append(query, 0, 1, 0, 1) // QTYPE=A, QCLASS=IN
	return query
}

// parseSingleAResponse extracts the address from a reply carrying exactly one
// A record. The rdata is the final four bytes of such a message, so the
// answer section needs no name-compression handling.
func parseSingleAResponse(msg []byte, txnID uint16) (net.IP, error) {
	if len(msg) < 16 {
		return nil, fmt.Errorf("reply too short: %d bytes", len(msg))
	}
	if got := binary.BigEndian.Uint16(msg[0:]); got != txnID {
		return nil, fmt.Errorf("transaction id %#04x, want %#04x", got, txnID)
	}
	if answers := binary.BigEndian.Uint16(msg[6:]); answers != 1 {
		return nil, fmt.Errorf("ANCOUNT %d, want exactly 1", answers)
	}
	return net.IP(msg[len(msg)-4:]).To4(), nil
}

// --- host-side receivers ----------------------------------------------------

func listenUDP(t *testing.T) (*net.UDPConn, uint16) {
	t.Helper()
	conn, err := net.ListenUDP("udp4", &net.UDPAddr{IP: net.IPv4(127, 0, 0, 1)})
	if err != nil {
		t.Fatalf("ListenUDP: %v", err)
	}
	t.Cleanup(func() { _ = conn.Close() })
	return conn, uint16(conn.LocalAddr().(*net.UDPAddr).Port)
}

func listenTCP(t *testing.T) (net.Listener, uint16) {
	t.Helper()
	ln, err := net.Listen("tcp4", "127.0.0.1:0")
	if err != nil {
		t.Fatalf("Listen: %v", err)
	}
	t.Cleanup(func() { _ = ln.Close() })
	return ln, uint16(ln.Addr().(*net.TCPAddr).Port)
}

// assertDatagramDropped fails unless the read deadline expires with nothing
// delivered. Only a timeout proves the filter dropped the datagram: any other
// read error (a closed socket, a bad deadline) would otherwise be mistaken for
// the policy working and turn every blocking test green for the wrong reason.
// probe describes the datagram that must not arrive.
func assertDatagramDropped(t *testing.T, conn *net.UDPConn, probeFormat string, probeArgs ...any) {
	t.Helper()
	if err := conn.SetReadDeadline(time.Now().Add(forwardWindow)); err != nil {
		t.Fatalf("SetReadDeadline: %v", err)
	}
	buf := make([]byte, 2048)
	n, from, err := conn.ReadFrom(buf)
	if err != nil {
		var netErr net.Error
		if errors.As(err, &netErr) && netErr.Timeout() {
			return // the deadline passed with no datagram, which is the contract
		}
		t.Fatalf("ReadFrom: %v", err)
	}
	t.Fatalf("%s was forwarded, arriving from %s: %q",
		fmt.Sprintf(probeFormat, probeArgs...), from, buf[:n])
}

// --- tests ------------------------------------------------------------------

// TestAllowNetBlocksUnlistedTCP is the control: it proves the allowlist is
// active and rejecting 198.51.100.9 on the transport that IS filtered.
// Without it, a UDP result could just mean a broken allow_net config.
func TestAllowNetBlocksUnlistedTCP(t *testing.T) {
	ln, port := listenTCP(t)
	tap := startNetwork(t, []string{allowedCIDR})

	accepted := make(chan net.Conn, 1)
	go func() {
		conn, err := ln.Accept()
		if err == nil {
			accepted <- conn
		}
	}()

	tap.send(t, tcpSynFrame(t, unlistedIP, port))

	select {
	case conn := <-accepted:
		_ = conn.Close()
		t.Fatalf("allow_net=%v: TCP to unlisted %s was forwarded to the host listener", allowedCIDR, unlistedIP)
	case <-time.After(forwardWindow):
	}
}

// TestAllowNetBlocksUnlistedUDP is the reproducer for the reported bypass:
// the same allowlist, the same unlisted destination, UDP instead of TCP.
func TestAllowNetBlocksUnlistedUDP(t *testing.T) {
	conn, port := listenUDP(t)
	tap := startNetwork(t, []string{allowedCIDR})

	tap.send(t, udpFrame(t, unlistedIP, port, []byte(probePayload)))

	assertDatagramDropped(t, conn, "allow_net=%v: UDP to unlisted %s", allowedCIDR, unlistedIP)
}

// TestAllowNetBlocksHostAliasUDP is the reproducer for the host-alias bypass.
// 192.168.127.254 NATs to the host's loopback, so it is an egress destination
// like any other: a guest reaching it under a restrictive allowlist would put
// every service bound to host loopback outside the reach of allow_net.
func TestAllowNetBlocksHostAliasUDP(t *testing.T) {
	cfg := testGvproxyConfig()
	conn, port := listenUDP(t)
	tap := startNetwork(t, []string{allowedCIDR})

	tap.send(t, udpFrame(t, cfg.HostIP, port, []byte(probePayload)))

	assertDatagramDropped(t, conn, "allow_net=%v: UDP to host alias %s", allowedCIDR, cfg.HostIP)
}

// TestAllowNetBlocksHostAliasTCP is the TCP twin: the same allowlist, the same
// host alias destination, over the transport that carries SNI/Host.
func TestAllowNetBlocksHostAliasTCP(t *testing.T) {
	cfg := testGvproxyConfig()
	ln, port := listenTCP(t)
	tap := startNetwork(t, []string{allowedCIDR})

	accepted := make(chan net.Conn, 1)
	go func() {
		conn, err := ln.Accept()
		if err == nil {
			accepted <- conn
		}
	}()

	tap.send(t, tcpSynFrame(t, cfg.HostIP, port))

	select {
	case conn := <-accepted:
		_ = conn.Close()
		t.Fatalf("allow_net=%v: TCP to host alias %s was forwarded to host loopback",
			allowedCIDR, cfg.HostIP)
	case <-time.After(forwardWindow):
	}
}

// TestAllowNetForwardsListedHostAlias pins the other half of the contract:
// once the alias is listed, policy still matches the pre-NAT address while the
// forwarder dials the NAT-translated loopback, so the host stays reachable.
func TestAllowNetForwardsListedHostAlias(t *testing.T) {
	cfg := testGvproxyConfig()
	conn, udpPort := listenUDP(t)
	ln, tcpPort := listenTCP(t)
	tap := startNetwork(t, []string{cfg.HostIP})

	accepted := make(chan net.Conn, 1)
	go func() {
		c, err := ln.Accept()
		if err == nil {
			accepted <- c
		}
	}()

	tap.send(t, udpFrame(t, cfg.HostIP, udpPort, []byte(probePayload)))
	if err := conn.SetReadDeadline(time.Now().Add(forwardWindow)); err != nil {
		t.Fatalf("SetReadDeadline: %v", err)
	}
	buf := make([]byte, 2048)
	n, _, err := conn.ReadFrom(buf)
	if err != nil {
		t.Fatalf("a listed host alias must forward UDP, but the datagram was dropped: %v", err)
	}
	if string(buf[:n]) != probePayload {
		t.Fatalf("forwarded payload = %q, want %q", buf[:n], probePayload)
	}

	tap.send(t, tcpSynFrame(t, cfg.HostIP, tcpPort))
	select {
	case c := <-accepted:
		_ = c.Close()
	case <-time.After(forwardWindow):
		t.Fatal("a listed host alias must forward TCP to host loopback")
	}
}

// TestAllowNetHostnameRulesAlsoBindUDP covers the hostname-only allowlist,
// where TCP policy comes from SNI/Host inspection that UDP has no analogue
// for. Half the test proves the DNS sinkhole is engaged; the other half
// sends UDP straight to a hard-coded IP, which is how a guest sidesteps it.
func TestAllowNetHostnameRulesAlsoBindUDP(t *testing.T) {
	cfg := testGvproxyConfig()
	conn, port := listenUDP(t)
	tap := startNetwork(t, []string{"example.com"})

	// The gateway resolver must sinkhole a name outside the allowlist,
	// otherwise the rest of this test proves nothing about policy being on.
	const txnID = 0xbe11
	tap.send(t, udpFrame(t, cfg.GatewayIP, 53, dnsQueryA(txnID, "blocked.test")))
	answer := tap.awaitUDPPayload(t, cfg.GatewayIP, 53)
	sinkholed, err := parseSingleAResponse(answer, txnID)
	if err != nil {
		t.Fatalf("gateway DNS reply: %v", err)
	}
	if !sinkholed.Equal(net.IPv4zero) {
		t.Fatalf("expected DNS sinkhole 0.0.0.0 for blocked.test, got %s", sinkholed)
	}

	// Same box, same policy: a datagram to a hard-coded IP never consults it.
	tap.send(t, udpFrame(t, unlistedIP, port, []byte(probePayload)))
	assertDatagramDropped(t, conn, "allow_net=[example.com]: UDP to hard-coded %s", unlistedIP)
}

// TestEmptyAllowlistForwardsUDP guards the other direction: an empty
// allow_net is documented as full internet access, so the new UDP handler
// must not turn the default configuration into a blackhole.
func TestEmptyAllowlistForwardsUDP(t *testing.T) {
	conn, port := listenUDP(t)
	tap := startNetwork(t, nil)

	tap.send(t, udpFrame(t, unlistedIP, port, []byte(probePayload)))

	if err := conn.SetReadDeadline(time.Now().Add(forwardWindow)); err != nil {
		t.Fatalf("SetReadDeadline: %v", err)
	}
	buf := make([]byte, 2048)
	n, _, err := conn.ReadFrom(buf)
	if err != nil {
		t.Fatalf("empty allow_net must forward UDP, but the datagram was dropped: %v", err)
	}
	if string(buf[:n]) != probePayload {
		t.Fatalf("forwarded payload = %q, want %q", buf[:n], probePayload)
	}
}

// TestUnfilteredNetworkForwardsUnlistedTCP pins the consequence of
// main.go:437 logging an OverrideTCPHandler failure and continuing: the box
// keeps the upstream default handler, which forwards everything. It is the
// same network minus the override, so a failed override is a fully open box.
func TestUnfilteredNetworkForwardsUnlistedTCP(t *testing.T) {
	ln, port := listenTCP(t)
	tap := startNetwork(t, nil) // no allow_net → no OverrideTCPHandler call

	accepted := make(chan net.Conn, 1)
	go func() {
		conn, err := ln.Accept()
		if err == nil {
			accepted <- conn
		}
	}()

	tap.send(t, tcpSynFrame(t, unlistedIP, port))

	select {
	case conn := <-accepted:
		_ = conn.Close()
	case <-time.After(forwardWindow):
		t.Fatal("default handler did not forward TCP — harness is not delivering frames to the stack")
	}
}
