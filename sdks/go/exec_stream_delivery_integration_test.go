//go:build boxlite_dev

package boxlite

import (
	"context"
	"sync"
	"testing"
	"time"
)

// blockingSink is an io.Writer that parks the goroutine calling Write until
// the test releases it. The first Write closes `entered` so the test can
// observe — without a wall-clock sleep — that delivery has reached the sink,
// then blocks on `release` (never sent during the test body) so the caller
// stays parked.
type blockingSink struct {
	entered     chan struct{}
	release     chan struct{}
	enteredOnce sync.Once
}

func newBlockingSink() *blockingSink {
	return &blockingSink{
		entered: make(chan struct{}),
		release: make(chan struct{}),
	}
}

func (s *blockingSink) Write(p []byte) (int, error) {
	s.enteredOnce.Do(func() { close(s.entered) })
	<-s.release
	return len(p), nil
}

// TestIntegrationStdoutBlockingSinkStallsRuntimeDrain guards against
// head-of-line blocking across executions on one Runtime. A first execution
// streams stdout into a sink that blocks forever; stream delivery must run off
// the shared drain goroutine (per-execution writer), so a second, trivial
// execution on the same box still completes promptly. With inline delivery on
// the single drain goroutine the second exec's Wait never dispatches and its
// context deadline fires instead.
func TestIntegrationStdoutBlockingSinkStallsRuntimeDrain(t *testing.T) {
	rt := newTestRuntime(t)
	box := createStartedBoxOrSkip(t, rt, "alpine:latest", WithAutoRemove(false))

	sink := newBlockingSink()

	// Execution A: emit one stdout chunk, then stay alive. The chunk is
	// delivered into `sink`, parking A's writer goroutine. We never Wait on A.
	execA, err := box.StartExecution(context.Background(), "sh", []string{"-c", "echo drain-wedge; sleep 60"}, &ExecutionOptions{
		Stdout: sink,
	})
	if err != nil {
		t.Fatalf("StartExecution(A): %v", err)
	}

	// Release the parked writer FIRST (close release), then free execution A.
	// Registered LAST so LIFO runs it BEFORE the box/runtime cleanups.
	t.Cleanup(func() {
		close(sink.release)
		_ = execA.Close()
	})

	// Wait (event, not a sleep) for delivery to actually reach the blocking
	// sink before probing.
	select {
	case <-sink.entered:
	case <-time.After(20 * time.Second):
		t.Skip("execution A never produced stdout within 20s; runtime/guest unavailable")
	}

	// Execution B: a trivial command on the SAME runtime. It must complete
	// regardless of A's blocking sink.
	ctxB, cancel := context.WithTimeout(context.Background(), 8*time.Second)
	defer cancel()

	start := time.Now()
	res, err := box.Exec(ctxB, "echo", "probe")
	elapsed := time.Since(start)

	if err != nil {
		t.Fatalf("HEAD-OF-LINE BLOCKING: a second exec on the same Runtime did not "+
			"complete in %s because execution A's blocking stdout sink wedged stream "+
			"delivery: %v", elapsed, err)
	}
	if res.Stdout != "probe\n" {
		t.Fatalf("probe exec returned unexpected stdout %q (want %q)", res.Stdout, "probe\n")
	}
}
