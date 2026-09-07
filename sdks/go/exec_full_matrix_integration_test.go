//go:build boxlite_dev

package boxlite

import (
	"bytes"
	"context"
	"testing"
	"time"
)

// TestIntegrationExecFullMatrix is the full termination x output-shape
// cross-product (8 x 3 = 24 cells) on a real box. For every combination,
// Execution.Wait must return promptly (no hang) with the right exit semantics
// and a sane byte count:
//   - clean terminations (normal / stderr / tty-normal) and kill terminations on
//     a fast shape (output finishes before the kill): exact full byte count
//     (except TTY, whose line discipline rewrites bytes);
//   - a kill that interrupts the slow trickle: a non-empty prefix (partial).
func TestIntegrationExecFullMatrix(t *testing.T) {
	rt := newTestRuntime(t)
	box := createStartedBoxOrSkip(t, rt, "alpine:latest", WithAutoRemove(false))

	type shape struct {
		name  string
		gen   string // sh fragment producing the output to stdout
		bytes int    // exact byte count when fully delivered (non-TTY)
		slow  bool   // runs longer than the kill deadlines (the trickle)
	}
	shapes := []shape{
		{"trickle", `i=0; while [ $i -lt 6 ]; do echo line-$i; sleep 0.5; i=$((i+1)); done`, 6 * len("line-0\n"), true},
		{"throughput", `awk 'BEGIN{for(i=0;i<200000;i++)print "0123456789abcdef0123456789abcdef"}'`, 200000 * 33, false},
		{"binary", `printf 'abcde'; head -c 1000 /dev/zero`, 1005, false},
	}

	type term struct {
		name      string
		wrap      func(gen string) string
		timeout   time.Duration
		kill      func(*Execution)
		killClass bool // exit must be non-zero; output may be a prefix
		wantExit  int  // asserted when !killClass
		tty       bool
		stderr    bool
		watchdog  time.Duration
	}
	terms := []term{
		{name: "normal", wrap: func(g string) string { return g + "; exit 7" }, wantExit: 7, watchdog: 25 * time.Second},
		{name: "timeout-SIGTERM", wrap: func(g string) string { return g + "; sleep 30" }, timeout: 2 * time.Second, killClass: true, watchdog: 25 * time.Second},
		{name: "hard-SIGKILL", wrap: func(g string) string { return "trap '' TERM; " + g + "; while :; do sleep 0.2; done" }, timeout: 2 * time.Second, killClass: true, watchdog: 35 * time.Second},
		{name: "bg-child", wrap: func(g string) string { return g + "; sleep 30 &" }, wantExit: 0, watchdog: 25 * time.Second},
		{name: "explicit-Kill", wrap: func(g string) string { return g + "; sleep 30" }, killClass: true, watchdog: 25 * time.Second,
			kill: func(e *Execution) { time.Sleep(1500 * time.Millisecond); _ = e.Kill(context.Background()) }},
		{name: "stderr", wrap: func(g string) string { return "{ " + g + "; } 1>&2; exit 3" }, wantExit: 3, stderr: true, watchdog: 25 * time.Second},
		{name: "tty-normal", wrap: func(g string) string { return g + "; exit 0" }, wantExit: 0, tty: true, watchdog: 25 * time.Second},
		{name: "tty-timeout", wrap: func(g string) string { return g + "; sleep 30" }, timeout: 2 * time.Second, killClass: true, tty: true, watchdog: 25 * time.Second},
	}

	for _, tm := range terms {
		for _, sh := range shapes {
			tm, sh := tm, sh
			t.Run(tm.name+"/"+sh.name, func(t *testing.T) {
				var out, errb bytes.Buffer
				opts := &ExecutionOptions{}
				switch {
				case tm.tty:
					opts.TTY = true
					opts.Stdout = &out
				case tm.stderr:
					opts.Stderr = &errb
				default:
					opts.Stdout = &out
				}
				opts.Timeout = tm.timeout

				exec, err := box.StartExecution(context.Background(), "sh", []string{"-c", tm.wrap(sh.gen)}, opts)
				if err != nil {
					t.Fatalf("StartExecution: %v", err)
				}
				defer exec.Close()
				if tm.kill != nil {
					go tm.kill(exec)
				}

				var code int
				var waitErr error
				waitWatchdog(t, tm.watchdog, t.Name(), func() { code, waitErr = exec.Wait(context.Background()) })
				if waitErr != nil {
					t.Fatalf("Wait: %v", waitErr)
				}

				if tm.killClass {
					if code == 0 {
						t.Errorf("exit = 0, want non-zero (killed)")
					}
				} else if code != tm.wantExit {
					t.Errorf("exit = %d, want %d", code, tm.wantExit)
				}

				n := out.Len()
				if tm.stderr {
					n = errb.Len()
				}
				if n == 0 {
					t.Fatalf("output empty, want bytes delivered before termination")
				}

				killInterruptsTrickle := tm.killClass && sh.slow
				switch {
				case tm.tty:
					// Line discipline rewrites bytes; only require non-empty.
				case killInterruptsTrickle:
					if n > sh.bytes {
						t.Errorf("partial output bytes = %d, exceeds full %d", n, sh.bytes)
					}
				default:
					if n != sh.bytes {
						t.Errorf("output bytes = %d, want exactly %d", n, sh.bytes)
					}
				}
			})
		}
	}
}
