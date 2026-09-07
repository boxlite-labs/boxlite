package boxlite

// Async cancel paths must reclaim the cgo.Handle (and any value carried
// to a cleanup hook). Without `abandonAsync` / `abandonAsyncErr` /
// `drainAndDelete`, a caller whose context cancelled before the C-side
// Tokio task completed would leak its `cgo.Handle` (and, for
// `Runtime.Create`, an entire live VM). These helpers move that cleanup
// onto a detached goroutine so the caller still returns ctx.Err()
// promptly AND the cgo-side resources are reclaimed.

import (
	"runtime/cgo"
	"testing"
	"time"
)

// expectAlreadyDeleted asserts that calling Delete on `h` panics, which is
// the documented evidence that the helper already deleted it. Polling the
// helper directly via Delete-and-recover is unsafe because the *first*
// Delete races the helper; instead the test sleeps a short interval to
// give the detached goroutine room to run, then asserts a follow-up Delete
// fails.
func expectAlreadyDeleted(t *testing.T, h cgo.Handle) {
	t.Helper()
	defer func() {
		if r := recover(); r == nil {
			t.Fatalf("cgo.Handle was NOT deleted by the helper")
		}
	}()
	h.Delete()
}

func TestAbandonAsync_RunsCleanupOnSuccess(t *testing.T) {
	ch := make(chan handleResult[int], 1)
	h := registerHandleForDispatch(cgo.NewHandle(ch))
	cleanupRan := make(chan int, 1)
	closing := make(chan struct{}) // never fires

	abandonAsync(ch, h, closing, func(v int) { cleanupRan <- v })

	// Simulate the eventual Tokio task completion.
	ch <- handleResult[int]{value: 42, err: nil}

	select {
	case v := <-cleanupRan:
		if v != 42 {
			t.Fatalf("cleanup got %d, want 42", v)
		}
	case <-time.After(time.Second):
		t.Fatal("cleanup did not run within 1s")
	}

	// Helper has already drained the channel (cleanup ran) — give it a
	// moment to also call Delete, then assert.
	time.Sleep(100 * time.Millisecond)
	expectAlreadyDeleted(t, h)
}

func TestAbandonAsync_SkipsCleanupOnError(t *testing.T) {
	ch := make(chan handleResult[int], 1)
	h := registerHandleForDispatch(cgo.NewHandle(ch))
	cleanupRan := make(chan int, 1)
	closing := make(chan struct{})

	abandonAsync(ch, h, closing, func(v int) { cleanupRan <- v })

	ch <- handleResult[int]{err: &Error{Code: ErrInternal, Message: "boom"}}

	// Cleanup should NOT run on error. Wait briefly to confirm.
	select {
	case v := <-cleanupRan:
		t.Fatalf("cleanup ran unexpectedly with value %d on error path", v)
	case <-time.After(100 * time.Millisecond):
		// expected
	}

	expectAlreadyDeleted(t, h)
}

func TestAbandonAsyncErr_DeletesHandle(t *testing.T) {
	ch := make(chan error, 1)
	h := registerHandleForDispatch(cgo.NewHandle(ch))
	closing := make(chan struct{})

	abandonAsyncErr(ch, h, closing)
	ch <- nil

	time.Sleep(100 * time.Millisecond)
	expectAlreadyDeleted(t, h)
}

func TestDrainAndDelete_DeletesHandle(t *testing.T) {
	ch := make(chan infoListResult, 1)
	h := registerHandleForDispatch(cgo.NewHandle(ch))
	closing := make(chan struct{})

	drainAndDelete(ch, h, closing)
	ch <- infoListResult{}

	time.Sleep(100 * time.Millisecond)
	expectAlreadyDeleted(t, h)
}

// ─── Close wakes detached cleanup goroutines ──────────────────────────────

func TestAbandonAsync_RespondsToCloseSignal(t *testing.T) {
	ch := make(chan handleResult[int], 1)
	h := registerHandleForDispatch(cgo.NewHandle(ch))
	closing := make(chan struct{})
	cleanupRan := make(chan int, 1)

	abandonAsync(ch, h, closing, func(v int) { cleanupRan <- v })

	// Close fires while ch is empty. The detached goroutine must wake on
	// `<-closing` instead of waiting forever for a result that the C side
	// will never deliver (because the runtime is closing).
	close(closing)

	// Cleanup MUST NOT run on the closing path — the runtime is going
	// away, all its boxes/images are about to be released by
	// boxlite_runtime_free anyway.
	select {
	case v := <-cleanupRan:
		t.Fatalf("cleanup ran on closing path with value %d; expected skip", v)
	case <-time.After(100 * time.Millisecond):
		// expected
	}

	expectAlreadyDeleted(t, h)
}

func TestAbandonAsyncErr_RespondsToCloseSignal(t *testing.T) {
	ch := make(chan error, 1)
	h := registerHandleForDispatch(cgo.NewHandle(ch))
	closing := make(chan struct{})

	abandonAsyncErr(ch, h, closing)
	close(closing)

	time.Sleep(100 * time.Millisecond)
	expectAlreadyDeleted(t, h)
}

func TestDrainAndDelete_RespondsToCloseSignal(t *testing.T) {
	ch := make(chan infoListResult, 1)
	h := registerHandleForDispatch(cgo.NewHandle(ch))
	closing := make(chan struct{})

	drainAndDelete(ch, h, closing)
	close(closing)

	time.Sleep(100 * time.Millisecond)
	expectAlreadyDeleted(t, h)
}

// ─── Per-execution cgo.Handle is deleted on Exit dispatch ─────────────────
//
// The stdout/stderr/exit callbacks share one per-execution `cgo.Handle`.
// To avoid Delete-after-use, `exit_pump` awaits each stream pump's
// `oneshot::Sender<()>` before pushing the Exit event, and `execution_free`
// synthesises an Exit on teardown so abort paths still terminate the
// dispatch chain. `goBoxliteOnExit` then calls `h.Delete()` on the way
// out, leaving exactly one Delete per handle. This test invokes the Exit
// dispatch directly and verifies the handle is deleted afterwards.

func TestExecutionStreamState_HandleDeletedOnExit(t *testing.T) {
	state := newExecutionStreamState(ExecutionOptions{}, nil)
	streamHandle := cgo.NewHandle(state)

	// Synthesize the C-side exit dispatch by calling the Go body of
	// goBoxliteOnExit. (We can't call goBoxliteOnExit directly from a
	// _test.go file because cgo isn't supported there; `dispatchExit` is
	// the test-friendly extraction.)
	dispatchExit(0, streamHandle)

	// goBoxliteOnExit deletes the handle on its way out.
	expectAlreadyDeleted(t, streamHandle)

	// State should also have observed the exit delivery.
	if !state.released.Load() {
		t.Fatal("executionStreamState.released was not set by deliverExit")
	}
}
