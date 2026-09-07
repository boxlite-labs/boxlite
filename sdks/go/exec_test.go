package boxlite

import (
	"reflect"
	"sync/atomic"
	"testing"
	"time"
)

// envMapToFlatPairs is the pure-Go pivot point of ExecutionOptions.Env
// support — it owns the contract that env_pairs lands in the C struct
// as a deterministic, even-length [k0,v0,k1,v1,...] sequence. The C
// side at sdks/c/src/exec/command.rs reads pairs in chunks of two
// and silently drops an odd trailing element, so any drift from the
// "sorted, flat, even-length" shape becomes a silent bug rather than
// a compile error. These cases pin the shape so a future refactor
// can't regress it unnoticed.
func TestEnvMapToFlatPairs(t *testing.T) {
	t.Run("nil yields nil", func(t *testing.T) {
		if got := envMapToFlatPairs(nil); got != nil {
			t.Fatalf("nil env must yield nil pairs, got %v", got)
		}
	})

	t.Run("empty yields nil", func(t *testing.T) {
		if got := envMapToFlatPairs(map[string]string{}); got != nil {
			t.Fatalf("empty env must yield nil pairs, got %v", got)
		}
	})

	t.Run("sorted by key, flat, even length", func(t *testing.T) {
		got := envMapToFlatPairs(map[string]string{
			"PATH":   "/usr/bin",
			"HOME":   "/root",
			"LANG":   "C",
			"DEBUG":  "1",
			"SHLVL":  "1",
			"PWD":    "/",
			"FOO":    "bar",
			"AAA":    "first",
			"ZZZ":    "last",
			"_UNDER": "score",
		})
		// 10 entries -> 20 strings.
		if len(got)%2 != 0 {
			t.Fatalf("flat pairs must have even length, got %d: %v", len(got), got)
		}
		want := []string{
			"AAA", "first",
			"DEBUG", "1",
			"FOO", "bar",
			"HOME", "/root",
			"LANG", "C",
			"PATH", "/usr/bin",
			"PWD", "/",
			"SHLVL", "1",
			"ZZZ", "last",
			"_UNDER", "score",
		}
		if !reflect.DeepEqual(got, want) {
			t.Fatalf("env pairs not in sorted key order\nwant: %v\n got: %v", want, got)
		}
	})

	t.Run("empty value preserved (env=) ", func(t *testing.T) {
		got := envMapToFlatPairs(map[string]string{"EMPTY": ""})
		want := []string{"EMPTY", ""}
		if !reflect.DeepEqual(got, want) {
			t.Fatalf("empty-value pair must round-trip, want %v got %v", want, got)
		}
	})
}

// TestWaitForStreamDrain pins Wait's drain-barrier semantics: it must keep
// waiting while the stream is still delivering (so a large tail is never cut
// off before the exit code is returned), give up promptly when a never-EOF
// pipe goes idle (instead of hanging forever), surface runtime close, and
// fall back to a hard cap.
func TestWaitForStreamDrain(t *testing.T) {
	t.Run("waits for a still-delivering stream then returns when drained", func(t *testing.T) {
		var progress atomic.Uint64
		drained := make(chan struct{})
		closing := make(chan struct{})
		// Deliver steadily for ~2.5s — past any 2s-style fixed cap — with
		// each gap (50ms) under idleBound (100ms) so it never looks idle,
		// then close drained. A fixed 2s cap would have bailed mid-stream.
		go func() {
			for i := 0; i < 50; i++ {
				time.Sleep(50 * time.Millisecond)
				progress.Add(1)
			}
			close(drained)
		}()

		start := time.Now()
		closed := waitForStreamDrain(drained, closing, &progress, 100*time.Millisecond, 30*time.Second)
		elapsed := time.Since(start)

		if closed {
			t.Fatal("expected closed=false: ended via drained, not runtime close")
		}
		if elapsed < 2400*time.Millisecond {
			t.Fatalf("returned after %v: gave up before the stream finished "+
				"delivering (a fixed 2s cap would do this)", elapsed)
		}
	})

	t.Run("gives up after one idle window on a stuck never-EOF pipe", func(t *testing.T) {
		var progress atomic.Uint64 // never bumped
		drained := make(chan struct{})
		closing := make(chan struct{})

		start := time.Now()
		closed := waitForStreamDrain(drained, closing, &progress, 100*time.Millisecond, 30*time.Second)
		elapsed := time.Since(start)

		if closed {
			t.Fatal("expected closed=false: idle stall, not runtime close")
		}
		if elapsed > 2*time.Second {
			t.Fatalf("took %v to give up on an idle stream; Wait would hang "+
				"that long on a SIGKILLed exec", elapsed)
		}
	})

	t.Run("reports runtime close", func(t *testing.T) {
		var progress atomic.Uint64
		drained := make(chan struct{})
		closing := make(chan struct{})
		close(closing)

		if closed := waitForStreamDrain(drained, closing, &progress, 100*time.Millisecond, 30*time.Second); !closed {
			t.Fatal("expected closed=true when the runtime is closing")
		}
	})

	t.Run("falls back to hard cap when a pipe trickles forever", func(t *testing.T) {
		var progress atomic.Uint64
		drained := make(chan struct{})
		closing := make(chan struct{})
		stop := make(chan struct{})
		defer close(stop)
		// Bump within every idle window forever so only the hard cap fires.
		go func() {
			tick := time.NewTicker(20 * time.Millisecond)
			defer tick.Stop()
			for {
				select {
				case <-stop:
					return
				case <-tick.C:
					progress.Add(1)
				}
			}
		}()

		start := time.Now()
		closed := waitForStreamDrain(drained, closing, &progress, 100*time.Millisecond, 300*time.Millisecond)
		elapsed := time.Since(start)

		if closed {
			t.Fatal("expected closed=false: hard cap, not runtime close")
		}
		if elapsed < 250*time.Millisecond || elapsed > 1500*time.Millisecond {
			t.Fatalf("expected release near the 300ms hard cap, got %v", elapsed)
		}
	})
}
