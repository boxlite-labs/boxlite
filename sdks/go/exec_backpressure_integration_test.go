//go:build boxlite_dev

package boxlite

import (
	"context"
	"runtime"
	"sync"
	"testing"
	"time"
)

// blockForeverSink blocks the writer on its first Write and never returns,
// modelling a stalled/blocked caller sink.
type blockForeverSink struct {
	once sync.Once
	rel  chan struct{}
}

func (b *blockForeverSink) Write(p []byte) (int, error) {
	b.once.Do(func() { <-b.rel })
	return len(p), nil
}

// TestIntegrationExecBackpressureBoundsMemory proves end-to-end stream
// back-pressure: a blocked caller sink on an infinitely-producing process must
// NOT cause unbounded host buffering. Without back-pressure the per-execution
// delivery queue grows without limit (the producer is never throttled); with
// it, the bounded Go queue pauses the C pump, which fills the bounded upstream
// channel, blocking the attach reader, the guest forwarder, and finally the
// guest process's write(). Heap growth must therefore plateau.
func TestIntegrationExecBackpressureBoundsMemory(t *testing.T) {
	rt := newTestRuntime(t)
	box := createStartedBoxOrSkip(t, rt, "alpine:latest", WithAutoRemove(false))

	sink := &blockForeverSink{rel: make(chan struct{})}
	exec, err := box.StartExecution(context.Background(), "sh",
		[]string{"-c", "yes ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789"}, &ExecutionOptions{Stdout: sink})
	if err != nil {
		t.Fatalf("StartExecution: %v", err)
	}
	t.Cleanup(func() { close(sink.rel); _ = exec.Close() })

	var m runtime.MemStats
	read := func() uint64 { runtime.ReadMemStats(&m); return m.HeapAlloc }

	// Let the producer run while the sink is blocked; sample heap growth.
	time.Sleep(1 * time.Second)
	base := read()
	for i := 0; i < 6; i++ {
		time.Sleep(500 * time.Millisecond)
	}
	grew := int64(read()) - int64(base)

	// With back-pressure the queue is capped near streamQueueHighWater (a few
	// MiB) plus the bounded in-flight buffers. A generous 64 MiB bound still
	// catches the unbounded regression (which grew tens of MiB/s indefinitely).
	const bound = 64 << 20
	if grew > bound {
		t.Fatalf("heap grew %d bytes over 3s under a blocked sink — stream "+
			"back-pressure is not bounding host memory (want plateau under %d)", grew, bound)
	}
}

// TestIntegrationExecStreamStallTimeout verifies the StreamStallTimeout safety
// net: when output is buffered but the caller's sink accepts nothing for the
// configured duration (a permanently blocked/abandoned sink), the execution is
// auto-killed so Wait returns and the box's resources are released — no manual
// stop required.
func TestIntegrationExecStreamStallTimeout(t *testing.T) {
	rt := newTestRuntime(t)
	box := createStartedBoxOrSkip(t, rt, "alpine:latest", WithAutoRemove(false))

	// A sink that blocks forever on the first write.
	rel := make(chan struct{})
	defer close(rel)
	sink := writerFunc(func(p []byte) (int, error) { <-rel; return len(p), nil })

	exec, err := box.StartExecution(context.Background(), "sh",
		[]string{"-c", "echo hi; sleep 600"},
		&ExecutionOptions{Stdout: sink, StreamStallTimeout: 3 * time.Second})
	if err != nil {
		t.Fatalf("StartExecution: %v", err)
	}
	defer exec.Close()

	type waitResult struct {
		code int
		err  error
	}
	done := make(chan waitResult, 1)
	go func() {
		c, e := exec.Wait(context.Background())
		done <- waitResult{c, e}
	}()
	start := time.Now()
	select {
	case res := <-done:
		// The stall-kill must surface through Wait: it returns cleanly (no
		// error) with the SIGKILL reflected in a non-zero exit code.
		if res.err != nil {
			t.Errorf("Wait err = %v, want nil (stall-kill surfaces via exit code)", res.err)
		}
		if res.code == 0 {
			t.Errorf("exit = 0, want non-zero (stall-killed)")
		}
		if elapsed := time.Since(start); elapsed > 10*time.Second {
			t.Errorf("stall-kill took %s, want ~3s", elapsed)
		}
	case <-time.After(20 * time.Second):
		t.Fatal("HANG: stalled exec was not auto-killed by StreamStallTimeout")
	}
}

// writerFunc adapts a func to io.Writer.
type writerFunc func([]byte) (int, error)

func (f writerFunc) Write(p []byte) (int, error) { return f(p) }
