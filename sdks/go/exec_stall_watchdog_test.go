package boxlite

import (
	"sync/atomic"
	"testing"
	"time"
)

// TestStallWatchdogKillsBufferedStall verifies the watchdog fires when output is
// buffered (queuedBytes > 0) yet the sink delivers nothing for the whole
// timeout, and that it waits at least one timeout before killing.
func TestStallWatchdogKillsBufferedStall(t *testing.T) {
	s := &executionStreamState{drained: make(chan struct{})}
	s.qmu.Lock()
	s.queuedBytes = 100 // buffered, never drains
	s.qmu.Unlock()

	const timeout = 200 * time.Millisecond
	var killed atomic.Bool
	done := make(chan struct{})
	start := time.Now()
	go func() {
		s.stallWatchdog(timeout, func() { killed.Store(true) })
		close(done)
	}()

	select {
	case <-done:
		elapsed := time.Since(start)
		if !killed.Load() {
			t.Fatal("watchdog returned without killing a permanently stalled sink")
		}
		// Anchored at stall-start: detection tick (~timeout/4) + a full timeout.
		if elapsed < timeout {
			t.Fatalf("killed after %v, want >= timeout (%v); fired too eagerly", elapsed, timeout)
		}
	case <-time.After(4 * timeout):
		t.Fatal("watchdog never killed a permanently stalled buffered sink")
	}
}

// TestStallWatchdogIgnoresQuietExec is the #7 regression guard: an execution
// that simply produces no output (queuedBytes stays 0, progress never advances)
// must never be killed — there is nothing buffered, so there is no stall. The
// earlier logic, which timed from the last progress regardless of whether
// anything was pending, would have killed this idle-but-healthy execution.
func TestStallWatchdogIgnoresQuietExec(t *testing.T) {
	s := &executionStreamState{drained: make(chan struct{})}
	// queuedBytes stays 0; progress is never bumped.

	const timeout = 150 * time.Millisecond
	var killed atomic.Bool
	go s.stallWatchdog(timeout, func() { killed.Store(true) })

	// Let several timeouts elapse: a buggy from-last-progress watchdog would
	// have fired by now.
	time.Sleep(4 * timeout)
	s.closeDrained() // clean end; let the watchdog exit

	if killed.Load() {
		t.Fatal("watchdog killed a quiet execution that buffered nothing")
	}
}
