package main

import (
	"encoding/binary"
	"errors"
	"fmt"
	"math"
	"math/bits"
	"net"
	"os"
	"sync"
	"sync/atomic"
	"syscall"
	"time"

	"github.com/sirupsen/logrus"
)

// Per-box bandwidth shaping for the guest link.
//
// The whole guest data plane is one AF_UNIX connection: libkrun's virtio-net
// backend on one side, gvisor-tap-vsock's tap.Switch on the other. The bridge
// owns that net.Conn right up until it hands it to vn.AcceptQemu/AcceptVfkit,
// which makes it the one place a rate limit can sit that is per-box, applies to
// both directions, and cannot be switched off from inside the guest. Shaping
// here needs no libkrun change and no gvisor upgrade.
//
// Direction names follow Firecracker's net device and are relative to the box:
// TX is what the box sends (guest -> internet), RX is what reaches it
// (internet -> guest). Note RX corresponds to the switch's *Sent* counter,
// because "sent" there means sent to the guest.
//
// Shaping below IP means one budget per direction covers TCP, UDP, ICMP and ARP
// alike. There is no per-protocol accounting, and none is wanted.

// maxFrameBytes is the largest single frame the shaper must be able to pass: the
// IPv4 TotalLength ceiling (65535) plus a 14-byte Ethernet header.
//
// Bucket capacity is floored here. On the RX side a whole frame is charged at
// once, so a smaller bucket could starve a maximum-size frame forever — the same
// class of deadlock as firecracker-microvm/firecracker#259.
//
// It matters on TX too: tap.Switch reads through a bufio.Reader
// (pkg/tap/switch.go:234), and bufio hands a read straight to the underlying
// conn when the request is at least its own buffer size and that buffer is
// empty, so a large frame body does arrive as a single Read.
const maxFrameBytes = 65549

// ---------------------------------------------------------------------------
// configuration
// ---------------------------------------------------------------------------

// TokenBucketConfig is Firecracker's bucket shape: a capacity in bytes, the time
// to refill it, and an optional one-shot burst spent before the sustained rate
// applies and never refilled.
//
// Sustained rate is Size * 1000 / RefillTimeMs bytes per second.
type TokenBucketConfig struct {
	Size         int64 `json:"size"`
	RefillTimeMs int64 `json:"refill_time_ms"`
	OneTimeBurst int64 `json:"one_time_burst,omitempty"`
}

// RateLimitConfig carries one bucket per direction. A nil or zero-Size
// direction is unlimited, matching Firecracker, Kata and Cloud Hypervisor.
type RateLimitConfig struct {
	RX *TokenBucketConfig `json:"rx,omitempty"`
	TX *TokenBucketConfig `json:"tx,omitempty"`
}

func (t *TokenBucketConfig) unlimited() bool { return t == nil || t.Size == 0 }

func (t *TokenBucketConfig) refillNs() int64 { return t.RefillTimeMs * int64(time.Millisecond) }

// bytesPerSec is only used for sizing the RX queue; the buckets themselves work
// straight off the (Size, RefillTimeMs) pair to avoid a rounding step.
func (t *TokenBucketConfig) bytesPerSec() int64 { return t.Size * 1000 / t.RefillTimeMs }

func (t *TokenBucketConfig) validate(dir string) error {
	if t.unlimited() {
		return nil
	}
	if t.Size < 0 {
		return fmt.Errorf("rate_limit.%s.size must not be negative (got %d)", dir, t.Size)
	}
	if t.RefillTimeMs <= 0 {
		return fmt.Errorf("rate_limit.%s.refill_time_ms must be positive (got %d)", dir, t.RefillTimeMs)
	}
	// refillNs() multiplies by 1e6 and runs before any of the guards in
	// refillLocked, so the bound has to live here. An hour is far beyond any
	// useful averaging window; the Rust core pins this to 100ms.
	if t.RefillTimeMs > 3_600_000 {
		return fmt.Errorf("rate_limit.%s.refill_time_ms must be at most 3600000 (got %d)", dir, t.RefillTimeMs)
	}
	// Guards bytesPerSec and the refill arithmetic below.
	if t.Size > (1<<62)/1000 {
		return fmt.Errorf("rate_limit.%s.size is too large (got %d)", dir, t.Size)
	}
	if t.OneTimeBurst < 0 {
		return fmt.Errorf("rate_limit.%s.one_time_burst must not be negative (got %d)", dir, t.OneTimeBurst)
	}
	return nil
}

// Validate rejects a partial or nonsensical configuration outright rather than
// letting half of it take effect — the same all-or-nothing rule Firecracker and
// E2B apply.
func (c *RateLimitConfig) Validate() error {
	if c == nil {
		return nil
	}
	if err := c.RX.validate("rx"); err != nil {
		return err
	}
	return c.TX.validate("tx")
}

func (c *RateLimitConfig) unlimited() bool {
	return c == nil || (c.RX.unlimited() && c.TX.unlimited())
}

// ---------------------------------------------------------------------------
// token bucket
// ---------------------------------------------------------------------------

// tokenBucket meters bytes against a configured rate.
//
// rateBytes and refillNs are the configured rate pair and are never adjusted;
// capacity — the ceiling on accumulated tokens, i.e. the burst size — is floored
// separately at maxFrameBytes. Keeping the two apart matters: raising capacity
// to the floor while deriving the rate from it would silently speed the limit up
// for any box configured below 64 KiB of burst.
type tokenBucket struct {
	mu        sync.Mutex
	rateBytes int64
	refillNs  int64
	capacity  int64
	tokens    int64
	oneTime   int64
	last      time.Time
	now       func() time.Time
}

// mulDiv returns a*b/d computed in 128 bits, and whether the result fits in
// int64. All three inputs are non-negative here.
//
// The naive form overflows for buckets past a few tens of gigabytes per refill
// period, and a wrapped intermediate is worse than an error: it silently turns
// the limit off instead of failing. Sizes that large are accepted by Validate
// and can come from a hand-written bridge config, so the arithmetic is made
// exact rather than assumed to be in range.
func mulDiv(a, b, d int64) (int64, bool) {
	hi, lo := bits.Mul64(uint64(a), uint64(b))
	if hi >= uint64(d) { // quotient would not fit in 64 bits
		return 0, false
	}
	q, _ := bits.Div64(hi, lo, uint64(d))
	if q > uint64(math.MaxInt64) {
		return 0, false
	}
	return int64(q), true
}

func newTokenBucket(cfg *TokenBucketConfig, now func() time.Time) *tokenBucket {
	if now == nil {
		now = time.Now
	}
	capacity := cfg.Size
	if capacity < maxFrameBytes {
		capacity = maxFrameBytes
	}
	return &tokenBucket{
		rateBytes: cfg.Size,
		refillNs:  cfg.refillNs(),
		capacity:  capacity,
		tokens:    capacity,
		oneTime:   cfg.OneTimeBurst,
		last:      now(),
		now:       now,
	}
}

// reserve refills from the clock, charges n bytes (letting the balance go
// negative) and reports how long the caller must wait for it to reach zero.
//
// It is pure policy: it never sleeps and never starts a goroutine, so the whole
// rate calculation is testable against an injected clock with exact equality
// rather than wall-clock tolerances.
func (b *tokenBucket) reserve(n int64) time.Duration {
	b.mu.Lock()
	defer b.mu.Unlock()

	b.refillLocked()

	if n > 0 {
		if b.oneTime > 0 {
			spend := n
			if spend > b.oneTime {
				spend = b.oneTime
			}
			b.oneTime -= spend
			n -= spend
		}
		b.tokens -= n
	}

	if b.tokens >= 0 {
		return 0
	}
	// Guarded like refillLocked: the deficit is normally one frame, but the
	// product is checked rather than assumed.
	wait, ok := mulDiv(-b.tokens, b.refillNs, b.rateBytes)
	if !ok {
		return time.Duration(math.MaxInt64)
	}
	return time.Duration(wait)
}

// waitNonNegative reports the wait left on an existing deficit without charging
// anything. This is the TX form: charge after reading, sleep off the overshoot
// before the next read.
func (b *tokenBucket) waitNonNegative() time.Duration { return b.reserve(0) }

// refillLocked converts elapsed time into tokens.
//
// Every product here is guarded before it is evaluated rather than bounded by
// what Validate happens to accept. refillNs*capacity overflows int64 once the
// bucket passes ~92 GB per refill period, and a wrapped negative threshold makes
// the "has a full period passed" test always true — which refills the bucket on
// every call and silently stops the limit from applying at all.
//
// These guards cover the arithmetic in this function. refillNs() itself is a
// separate multiplication that happens earlier, so its input is bounded in
// Validate instead.
func (b *tokenBucket) refillLocked() {
	now := b.now()
	elapsed := now.Sub(b.last)
	if elapsed <= 0 {
		return
	}
	elapsedNs := elapsed.Nanoseconds()

	gained, ok := mulDiv(elapsedNs, b.rateBytes, b.refillNs)
	if !ok || (b.tokens >= 0 && gained >= b.capacity-b.tokens) {
		// Clamp before adding to avoid overflow. A negative balance must
		// first repay its debt, even when gained exceeds one full bucket.
		b.tokens = b.capacity
		b.last = now
		return
	}
	if gained <= 0 {
		// Not a whole token yet. Leave `last` alone so repeated sub-token calls
		// do not discard the remainder.
		return
	}
	b.tokens += gained
	if b.tokens > b.capacity {
		b.tokens = b.capacity
	}
	// Advance `last` by exactly the time those tokens represent, keeping the
	// sub-token remainder for next time.
	usedNs, ok := mulDiv(gained, b.refillNs, b.rateBytes)
	if !ok {
		b.last = now
		return
	}
	b.last = b.last.Add(time.Duration(usedNs))
}

// ---------------------------------------------------------------------------
// frame boundary tracking
// ---------------------------------------------------------------------------

// framer follows frame boundaries in a byte stream without reframing or
// buffering it. It answers one question: of these n bytes, how many were frame
// body rather than length prefix.
//
// It exists so the buckets charge pure frame bytes, matching both the switch's
// own counters (pkg/tap/switch.go:152,167 charge pkt.Size(); :291 charges
// len(buf), neither including the prefix) and Firecracker/gVisor, which meter
// below any transport framing. Charging raw socket bytes instead would bill
// Linux four extra bytes per frame and macOS none: 0.26% on a 1514-byte frame,
// but 5.7% on a stream of 66-byte ACKs, and inconsistent across platforms
// either way.
//
// hdrLen comes from the caller and is never assumed: gvisor-tap-vsock's
// pkg/tap/protocols.go uses four big-endian bytes for qemu, two little-endian
// for hyperkit, and no prefix at all for the datagram protocols.
type framer struct {
	hdrLen int
	hdrGot int
	hdrBuf [4]byte
	body   int
}

func (f *framer) consume(b []byte) (frameBytes, frames int) {
	if len(b) == 0 {
		return 0, 0
	}
	// Datagram protocols carry no prefix: one read is exactly one frame.
	if f.hdrLen == 0 {
		return len(b), 1
	}

	i := 0
	for i < len(b) {
		if f.body == 0 {
			need := f.hdrLen - f.hdrGot
			n := len(b) - i
			if n > need {
				n = need
			}
			copy(f.hdrBuf[f.hdrGot:], b[i:i+n])
			f.hdrGot += n
			i += n
			if f.hdrGot == f.hdrLen {
				f.body = f.decodeLen()
				f.hdrGot = 0
				if f.body == 0 {
					frames++
				}
			}
			continue
		}
		n := len(b) - i
		if n > f.body {
			n = f.body
		}
		f.body -= n
		frameBytes += n
		i += n
		if f.body == 0 {
			frames++
		}
	}
	return frameBytes, frames
}

// decodeLen mirrors pkg/tap/protocols.go exactly: qemu writes a big-endian
// uint32, hyperkit a little-endian uint16.
func (f *framer) decodeLen() int {
	if f.hdrLen == 2 {
		return int(binary.LittleEndian.Uint16(f.hdrBuf[:2]))
	}
	return int(binary.BigEndian.Uint32(f.hdrBuf[:4]))
}

// ---------------------------------------------------------------------------
// counters
// ---------------------------------------------------------------------------

// shaperStats stays internal to the bridge for now. The live stats path (GET
// /stats over the control socket) serves vn.ServicesMux() directly, so there is
// no bridge-owned mux to inject into, and the FFI stats path has no callers on
// the Rust side. Surfacing these belongs with that plumbing, not here.
type shaperStats struct {
	txThrottledEvents atomic.Int64
	txThrottledNs     atomic.Int64
	rxThrottledEvents atomic.Int64
	rxThrottledNs     atomic.Int64
	rxDroppedFrames   atomic.Int64
	rxDroppedBytes    atomic.Int64
	rxDeliveredBytes  atomic.Int64
}

type shaperSnapshot struct {
	TxThrottledEvents int64
	TxThrottledNs     int64
	RxThrottledEvents int64
	RxThrottledNs     int64
	RxDroppedFrames   int64
	RxDroppedBytes    int64
	RxDeliveredBytes  int64
}

func (s *shaperStats) snapshot() shaperSnapshot {
	return shaperSnapshot{
		TxThrottledEvents: s.txThrottledEvents.Load(),
		TxThrottledNs:     s.txThrottledNs.Load(),
		RxThrottledEvents: s.rxThrottledEvents.Load(),
		RxThrottledNs:     s.rxThrottledNs.Load(),
		RxDroppedFrames:   s.rxDroppedFrames.Load(),
		RxDroppedBytes:    s.rxDroppedBytes.Load(),
		RxDeliveredBytes:  s.rxDeliveredBytes.Load(),
	}
}

// ---------------------------------------------------------------------------
// shaped connection
// ---------------------------------------------------------------------------

type shapedConn struct {
	net.Conn

	hdrLen int
	tx     *tokenBucket
	rx     *tokenBucket
	framer framer // read side only; tap.Switch reads from a single goroutine

	queue      chan []byte
	queueBytes atomic.Int64
	maxQueueB  int64

	readDeadlineMu      sync.Mutex
	readDeadline        time.Time
	readDeadlineChanged chan struct{}

	stop      chan struct{}
	closeOnce sync.Once
	stats     shaperStats
}

// wrapConn is the single place a connection becomes shaped; main.go and the test
// harness both go through it so the two cannot drift.
//
// An unlimited configuration returns the connection untouched, so a box with no
// limit pays nothing, not even an interface indirection.
func wrapConn(conn net.Conn, hdrLen int, cfg *RateLimitConfig) net.Conn {
	if cfg.unlimited() {
		return conn
	}
	return newShapedConn(conn, hdrLen, cfg, nil)
}

func newShapedConn(conn net.Conn, hdrLen int, cfg *RateLimitConfig, now func() time.Time) *shapedConn {
	c := &shapedConn{
		Conn:                conn,
		hdrLen:              hdrLen,
		framer:              framer{hdrLen: hdrLen},
		stop:                make(chan struct{}),
		readDeadlineChanged: make(chan struct{}),
	}
	if !cfg.TX.unlimited() {
		c.tx = newTokenBucket(cfg.TX, now)
	}
	if !cfg.RX.unlimited() {
		c.rx = newTokenBucket(cfg.RX, now)
		queueBytes, queueFrames := queueBounds(cfg.RX)
		c.maxQueueB = queueBytes
		c.queue = make(chan []byte, queueFrames)
		go c.pace()
	}
	return c
}

// queueBounds sizes the RX queue from the configured rate: one hundred
// milliseconds of buffering, the same time constant the bucket refill uses.
//
// The floor is two maximum frames, and that is a capacity requirement, not a
// latency target: a queue that cannot hold one maximum-size frame would drop it
// unconditionally, so the floor is what makes a full-size frame deliverable at
// any rate.
//
// It does cost latency at low rates — 2*maxFrameBytes is ~1s of standing queue
// at 1 Mbit/s and ~16s at 64 kbit/s, so a slow box trades round-trip time for
// the ability to carry large frames at all. A larger floor would be worse (eight
// frames is ~4s at 1 Mbit/s) but no floor is not an option. Active queue
// management (CoDel) is the principled fix for the latency half and is out of
// scope here.
func queueBounds(cfg *TokenBucketConfig) (queueBytes int64, queueFrames int) {
	queueBytes = cfg.bytesPerSec() / 10
	if min := int64(2 * maxFrameBytes); queueBytes < min {
		queueBytes = min
	}
	if max := int64(4 << 20); queueBytes > max {
		queueBytes = max
	}
	queueFrames = int(queueBytes / 1514)
	if queueFrames < 16 {
		queueFrames = 16
	}
	if queueFrames > 4096 {
		queueFrames = 4096
	}
	return queueBytes, queueFrames
}

// sleep waits for d, or reports false if the connection was closed first. Every
// wait in the shaper goes through here: a bare time.Sleep would strand Read or
// the pacer for a full refill window after Close.
func (c *shapedConn) sleep(d time.Duration) bool {
	if d <= 0 {
		return true
	}
	t := time.NewTimer(d)
	defer t.Stop()
	select {
	case <-c.stop:
		return false
	case <-t.C:
		return true
	}
}

// SetReadDeadline covers both the underlying read and the TX token wait.
// Serialize forwarding and notification so concurrent setters cannot leave
// the wrapper and the underlying connection with different read deadlines.
func (c *shapedConn) SetReadDeadline(t time.Time) error {
	c.readDeadlineMu.Lock()
	defer c.readDeadlineMu.Unlock()
	if err := c.Conn.SetReadDeadline(t); err != nil {
		return err
	}
	c.setReadDeadlineLocked(t)
	return nil
}

func (c *shapedConn) SetDeadline(t time.Time) error {
	c.readDeadlineMu.Lock()
	defer c.readDeadlineMu.Unlock()
	if err := c.Conn.SetDeadline(t); err != nil {
		return err
	}
	c.setReadDeadlineLocked(t)
	return nil
}

func (c *shapedConn) setReadDeadlineLocked(t time.Time) {
	c.readDeadline = t
	close(c.readDeadlineChanged)
	c.readDeadlineChanged = make(chan struct{})
}

func (c *shapedConn) waitForTX() error {
	d := c.tx.waitNonNegative()
	if d <= 0 {
		return nil
	}
	c.stats.txThrottledEvents.Add(1)
	c.stats.txThrottledNs.Add(int64(d))

	// Deadline updates wake the waiter without restarting the token wait.
	readyAt := time.Now().Add(d)
	timer := time.NewTimer(d)
	defer timer.Stop()
	for {
		select {
		case <-c.stop:
			return net.ErrClosed
		default:
		}
		c.readDeadlineMu.Lock()
		deadline, changed := c.readDeadline, c.readDeadlineChanged
		c.readDeadlineMu.Unlock()

		now := time.Now()
		wait := readyAt.Sub(now)
		if !deadline.IsZero() {
			remaining := deadline.Sub(now)
			if remaining <= 0 {
				return &net.OpError{Op: "read", Err: os.ErrDeadlineExceeded}
			}
			if remaining < wait {
				wait = remaining
			}
		}
		if wait <= 0 {
			return nil
		}
		timer.Reset(wait)
		select {
		case <-c.stop:
			return net.ErrClosed
		case <-changed:
		case <-timer.C:
		}
	}
}

// Read shapes TX, what the box sends.
//
// The charge happens after the read, not before, so there is no need to guess
// how many bytes the read will return: the balance simply goes negative and the
// next call sleeps off the overshoot. That overshoot is bounded by one Read,
// the same bound the capacity floor already assumes, so TX cannot starve at any
// bucket size. RX is charged up front, so it could starve on a frame larger than
// the bucket — that is what the maxFrameBytes capacity floor in newTokenBucket
// prevents, and it is the only guard against that deadlock class.
//
// Not reading is itself the backpressure: the AF_UNIX receive buffer fills, and
// on Linux libkrun's unixstream backend then sees EAGAIN, defers the frame and
// stops draining the virtio tx queue, so the guest's own TCP backs off. The
// macOS unixgram backend has no equivalent mechanism and its behaviour under a
// full receive buffer is unverified — do not assume it backpressures.
func (c *shapedConn) Read(p []byte) (int, error) {
	if c.tx != nil {
		if err := c.waitForTX(); err != nil {
			return 0, err
		}
	}
	n, err := c.Conn.Read(p)
	if n > 0 && c.tx != nil {
		frameBytes, _ := c.framer.consume(p[:n])
		c.tx.reserve(int64(frameBytes))
	}
	return n, err
}

// Write shapes RX, what reaches the box, by queueing rather than blocking.
//
// Two upstream facts make queue-and-drop the only legal strategy here:
//
//   - tap.Switch.txPkt holds both writeLock and connLock across the whole
//     conn.Write call (pkg/tap/switch.go:119-124), so blocking here wedges the
//     entire switch, Accept and disconnect included.
//   - tap.Switch.txBuf treats any error other than ENOBUFS as fatal and calls
//     e.disconnect (pkg/tap/switch.go:186), tearing down the VM's connection, so
//     returning an error here would kill networking outright.
//
// Cannot block, cannot fail: a full queue must drop and count.
//
// The frame must be copied. switch.go:126 hands over pkt.ToView().AsSlice(), and
// rxBuf releases the packet as soon as tx returns; on the datagram path txBuf
// does not reallocate (its append runs only for stream protocols, :173-177), so
// the slice is gvisor-owned and may be recycled the moment this returns.
// Queueing it uncopied would be a use-after-free-shaped race. Allocating per
// frame is fine without a pool: the queue is bounded and drains at the
// configured rate, so churn is capped by the limit the user asked for.
func (c *shapedConn) Write(p []byte) (int, error) {
	if c.rx == nil {
		return c.Conn.Write(p)
	}

	size := int64(len(p))
	if c.queueBytes.Load()+size > c.maxQueueB {
		c.dropped(size)
		return len(p), nil
	}

	frame := make([]byte, len(p))
	copy(frame, p)

	select {
	case c.queue <- frame:
		c.queueBytes.Add(size)
	default:
		c.dropped(size)
	}
	return len(p), nil
}

func (c *shapedConn) dropped(size int64) {
	c.stats.rxDroppedFrames.Add(1)
	c.stats.rxDroppedBytes.Add(size)
}

func (c *shapedConn) dequeue(frame []byte) {
	c.queueBytes.Add(-int64(len(frame)))
}

// pace drains the RX queue at the configured rate.
func (c *shapedConn) pace() {
	for {
		select {
		case <-c.stop:
			c.drain()
			return
		case frame := <-c.queue:
			c.dequeue(frame)

			// The frame arrives whole, so the only adjustment needed is to
			// leave the length prefix out of the charge.
			charge := int64(len(frame))
			if charge > int64(c.hdrLen) {
				charge -= int64(c.hdrLen)
			}
			if d := c.rx.reserve(charge); d > 0 {
				c.stats.rxThrottledEvents.Add(1)
				c.stats.rxThrottledNs.Add(int64(d))
				if !c.sleep(d) {
					c.drain()
					return
				}
			}
			if !c.writeFrame(frame) {
				return
			}
			c.stats.rxDeliveredBytes.Add(charge)
		}
	}
}

// writeFrame reports whether the pacer should keep running.
func (c *shapedConn) writeFrame(frame []byte) bool {
	for {
		if _, err := c.Conn.Write(frame); err != nil {
			if errors.Is(err, syscall.ENOBUFS) {
				// The same retry upstream performs (gvisor-tap-vsock#367), but
				// paced: a bare continue spins a core while the buffer drains.
				if !c.sleep(200 * time.Microsecond) {
					return false
				}
				continue
			}
			// Closing the inner conn makes rxStream's ReadFull fail, which
			// returns Switch.Accept and lets the library run its own deferred
			// disconnect. Reaching into the switch from here is neither possible
			// (disconnect is unexported) nor desirable.
			_ = c.Conn.Close()
			return false
		}
		return true
	}
}

func (c *shapedConn) drain() {
	for {
		select {
		case frame := <-c.queue:
			c.dequeue(frame)
			c.dropped(int64(len(frame)))
		default:
			return
		}
	}
}

// Close must never wait for the pacer.
//
// tap.Switch.txBuf calls e.disconnect while txPkt already holds writeLock and
// connLock (pkg/tap/switch.go:119-124, :186), and disconnect closes the
// connection (:202); Accept's deferred cleanup reaches the same call under
// connLock (:92-96). So this runs with the switch's global write lock held. If
// it joined a pacer that happened to be blocked in Conn.Write on a full socket
// buffer, the entire switch would deadlock behind that lock.
//
// Closing the inner conn is what actually unblocks an in-flight pacer write,
// with net.ErrClosed; the pacer then observes stop, discards the queue and exits
// on its own.
func (c *shapedConn) Close() error {
	var err error
	c.closeOnce.Do(func() {
		close(c.stop)
		err = c.Conn.Close()

		// The only reader of these counters. There is no path from here to the
		// control socket's /stats (it serves vn.ServicesMux directly, with no
		// bridge-owned mux to inject into), so without this line an RX queue
		// overflow would leave no trace anywhere.
		s := c.stats.snapshot()
		logrus.WithFields(logrus.Fields{
			"tx_throttled_events": s.TxThrottledEvents,
			"tx_throttled_ms":     s.TxThrottledNs / int64(time.Millisecond),
			"rx_throttled_events": s.RxThrottledEvents,
			"rx_throttled_ms":     s.RxThrottledNs / int64(time.Millisecond),
			"rx_delivered_bytes":  s.RxDeliveredBytes,
			"rx_dropped_frames":   s.RxDroppedFrames,
			"rx_dropped_bytes":    s.RxDroppedBytes,
			"rx_queue_bytes":      c.queueBytes.Load(),
		}).Debug("shaped conn closed")
	})
	return err
}
