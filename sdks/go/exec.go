package boxlite

/*
#include "bridge.h"
#include <stdlib.h>
*/
import "C"
import (
	"bytes"
	"context"
	"io"
	"runtime/cgo"
	"sort"
	"sync"
	"sync/atomic"
	"time"
	"unsafe"
)

// ExecResult contains the result of a buffered command execution.
type ExecResult struct {
	ExitCode int
	Stdout   string
	Stderr   string
}

// envMapToFlatPairs flattens an env map into a [k0, v0, k1, v1, ...]
// slice sorted by key. Sorting makes the C call deterministic across
// runs, which matters for test reproducibility and for any downstream
// hashing the runtime might do over the pairs.
func envMapToFlatPairs(env map[string]string) []string {
	if len(env) == 0 {
		return nil
	}
	keys := make([]string, 0, len(env))
	for k := range env {
		keys = append(keys, k)
	}
	sort.Strings(keys)
	pairs := make([]string, 0, 2*len(env))
	for _, k := range keys {
		pairs = append(pairs, k, env[k])
	}
	return pairs
}

// envMapToCStringArray converts an env map to a C string array suitable
// for `BoxliteCommand.env_pairs`. Returns (nil, 0) for nil/empty input.
// Caller must free via freeCStringArray.
func envMapToCStringArray(env map[string]string) (**C.char, int) {
	return toCStringArray(envMapToFlatPairs(env))
}

// ExecutionOptions configures a streaming command execution.
type ExecutionOptions struct {
	TTY      bool
	Stdout   io.Writer
	Stderr   io.Writer
	OnStdout func([]byte)
	OnStderr func([]byte)
	// Env is the environment given to the executed process. nil/empty
	// inherits the container default. The map is serialised into a flat
	// [k0, v0, k1, v1, ...] C array in deterministic key order.
	Env map[string]string
	// WorkingDir is the directory the process starts in. Empty inherits
	// the container default.
	WorkingDir string
	// Timeout bounds the wall-clock lifetime of the execution. Zero
	// means no timeout (the C side treats `timeout_secs <= 0` as
	// unbounded — see `sdks/c/src/exec/command.rs`).
	Timeout time.Duration
	// StreamStallTimeout auto-kills the execution if back-pressure has fully
	// stalled — i.e. there is output buffered but the caller's Stdout/Stderr
	// sink has accepted nothing for this long. It guards against a permanently
	// blocked/abandoned sink leaving the producer suspended (and the box's
	// resources held) forever. A sink that keeps making progress, however
	// slowly, is never killed; an execution that simply produces no output is
	// never killed (nothing is buffered). Zero disables the guard.
	StreamStallTimeout time.Duration
}

// executionStreamState holds the user-provided sinks for streaming output
// plus a "released" flag. A single instance is shared between the stdout,
// stderr, and exit callback registrations on the C side.
//
// Lifetime: the cgo.Handle wrapping this state is intentionally leaked for
// the lifetime of the Runtime. Stream events (stdout/stderr) and the exit
// event are pushed concurrently by independent Rust pumps with no global
// ordering guarantee between them, so deleting the handle from any one
// callback could race a sibling callback's value lookup. The released
// flag short-circuits any post-exit stream events that still arrive.
//
// Memory overhead is bounded: each execution adds one map entry plus the
// state struct (a few writers, two atomics, and a mutex).
type executionStreamState struct {
	stdout   io.Writer
	stderr   io.Writer
	onStdout func([]byte)
	onStderr func([]byte)

	released atomic.Bool

	// drained is the os/exec `goroutineErr` analog: closed by
	// deliverExit when the C-side on_exit callback fires. C's exit_pump
	// gates Exit on every stream pump's done_tx, so by the time on_exit
	// runs, the drain goroutine has already dispatched every
	// Stdout/Stderr callback for this execution. Execution.Wait blocks
	// on this after the process's exit code has been collected, so a
	// caller using buffered Stdout/Stderr sinks never sees a truncated
	// buffer. The fold happens at the SDK boundary; the C-side Wait
	// task stays decoupled from streams.
	drained chan struct{}

	// progress is a monotonic count of Stdout/Stderr callbacks delivered.
	// Wait's drain barrier watches it: a SIGKILLed guest can leave a pipe
	// without EOF, so the C exit pump never fires on_exit and `drained`
	// never closes — bounding the barrier by progress lets Wait return
	// (the stream has stalled) instead of hanging forever. While output is
	// still flowing progress keeps advancing, so a large tail is never cut
	// off early.
	progress atomic.Uint64

	// Stream output is delivered to the caller's sinks by a dedicated
	// per-execution goroutine (deliverLoop), NOT inline on the shared
	// per-Runtime drain goroutine. deliverStdout/deliverStderr are invoked on
	// that single drain goroutine (runtime.go drainLoop -> C dispatch); writing
	// to a caller-supplied Writer inline there lets one execution's slow or
	// blocking sink wedge the drain goroutine and stall delivery — including
	// Wait completions — for EVERY execution on the same Runtime
	// (head-of-line blocking). Enqueue is non-blocking; the writer goroutine
	// absorbs sink back-pressure in isolation.
	qmu         sync.Mutex
	qcond       *sync.Cond
	queue       []streamChunk
	exited      bool // on_exit seen: flush the queue, then close drained
	stopped     bool // execution released/closed: stop the writer
	drainedOnce sync.Once

	// Bounded-queue back-pressure. queuedBytes is the bytes buffered but not
	// yet written to the caller's sink. When it crosses the high-water mark the
	// writer is falling behind, so we pause the C-side pump (which fills the
	// bounded upstream channel and ultimately blocks the guest process's
	// write()); we resume once it drains below the low-water mark. `pause` calls
	// boxlite_execution_set_stream_paused under qmu — Close sets `stopped` under
	// qmu before freeing the handle, so a pause call can never use it post-free.
	queuedBytes int
	paused      bool
	pause       func(bool)
}

const (
	// streamQueueHighWater: buffered bytes at which the writer is deemed behind
	// and the producer is paused. streamQueueLowWater: resume threshold. The gap
	// is hysteresis so a steady stream doesn't thrash pause/resume.
	streamQueueHighWater = 4 << 20 // 4 MiB
	streamQueueLowWater  = 1 << 20 // 1 MiB
)

// streamChunk is one ordered unit of stream output queued for async delivery.
type streamChunk struct {
	stderr bool
	data   []byte
}

// newExecutionStreamState builds the stream state. setPaused, when non-nil, is
// the raw C call that pauses/resumes the execution's pumps; it is wrapped so it
// only runs while the handle is alive (guarded by qmu against Close's free).
func newExecutionStreamState(opts ExecutionOptions, setPaused func(bool)) *executionStreamState {
	s := &executionStreamState{
		stdout:   opts.Stdout,
		stderr:   opts.Stderr,
		onStdout: opts.OnStdout,
		onStderr: opts.OnStderr,
		drained:  make(chan struct{}),
	}
	s.qcond = sync.NewCond(&s.qmu)
	if setPaused != nil {
		s.pause = func(p bool) {
			// Call the C FFI while holding qmu and only when not stopped.
			// markReleased sets `stopped` under qmu before Close frees the
			// handle, so this can never touch a freed handle.
			s.qmu.Lock()
			if !s.stopped {
				setPaused(p)
			}
			s.qmu.Unlock()
		}
	}
	go s.deliverLoop()
	return s
}

// deliverStdout/deliverStderr run on the shared per-Runtime drain goroutine.
// They only enqueue — never write to the caller's sink — so a blocking sink
// cannot wedge the drain goroutine (see executionStreamState). The bytes are
// already a Go-owned copy (C.GoBytes at the cgo boundary), so the slice is safe
// to retain in the queue.
func (s *executionStreamState) deliverStdout(data []byte) { s.enqueue(streamChunk{false, data}) }
func (s *executionStreamState) deliverStderr(data []byte) { s.enqueue(streamChunk{true, data}) }

func (s *executionStreamState) enqueue(c streamChunk) {
	if s.released.Load() {
		return
	}
	doPause := false
	s.qmu.Lock()
	if !s.exited && !s.stopped {
		s.queue = append(s.queue, c)
		s.queuedBytes += len(c.data)
		if s.queuedBytes > streamQueueHighWater && !s.paused {
			s.paused = true
			doPause = true
		}
		s.qcond.Signal()
	}
	s.qmu.Unlock()
	if doPause && s.pause != nil {
		s.pause(true)
	}
}

// deliverExit marks end-of-stream. The writer goroutine flushes whatever is
// still queued (on_exit fires only after every stream callback for this
// execution, so the queue holds the full tail) and then closes drained, which
// Execution.Wait's drain barrier is waiting on.
func (s *executionStreamState) deliverExit(_ int) {
	s.released.Store(true)
	s.qmu.Lock()
	s.exited = true
	s.qcond.Signal()
	s.qmu.Unlock()
}

func (s *executionStreamState) markReleased() {
	s.released.Store(true)
	s.qmu.Lock()
	s.stopped = true
	s.qcond.Signal()
	s.qmu.Unlock()
}

// deliverLoop is the per-execution writer goroutine. It drains the queue in
// FIFO order (preserving stdout/stderr interleaving) into the caller's sinks,
// bumping progress per chunk so Wait's progress-bounded drain barrier can tell
// a still-delivering stream from a stalled one. Blocking here isolates sink
// back-pressure to this one execution. Exits — closing drained — once on_exit
// has been seen and the queue is empty, or when the execution is released.
func (s *executionStreamState) deliverLoop() {
	defer s.closeDrained()
	for {
		s.qmu.Lock()
		for len(s.queue) == 0 && !s.exited && !s.stopped {
			s.qcond.Wait()
		}
		q := s.queue
		s.queue = nil
		done := s.exited || s.stopped
		s.qmu.Unlock()

		batch := 0
		for _, c := range q {
			s.writeChunk(c)
			batch += len(c.data)
		}
		// Account the drained bytes; resume the producer once we're back under
		// the low-water mark (only while live — on exit/stop the pump is torn
		// down anyway, and resuming there could race the handle free).
		doResume := false
		s.qmu.Lock()
		s.queuedBytes -= batch
		if s.paused && !done && s.queuedBytes < streamQueueLowWater {
			s.paused = false
			doResume = true
		}
		s.qmu.Unlock()
		if doResume && s.pause != nil {
			s.pause(false)
		}
		if done {
			// No more chunks can arrive once exited/stopped is set (deliver*
			// calls are serialized on the single drain goroutine), but a chunk
			// enqueued before the flag was observed may remain — flush it.
			s.qmu.Lock()
			rest := s.queue
			s.queue = nil
			s.qmu.Unlock()
			for _, c := range rest {
				s.writeChunk(c)
			}
			return
		}
	}
}

// writeChunk hands one chunk to the caller's sink. Write errors are
// deliberately not propagated: this runs on the async delivery goroutine with
// no caller to return them to, and a transient sink hiccup should not tear down
// a healthy execution. A sink that fails permanently simply stops accepting
// bytes, which leaves output buffered and is exactly the condition
// StreamStallTimeout watches for and kills — so a wedged sink is still handled,
// without making every transient write error fatal.
func (s *executionStreamState) writeChunk(c streamChunk) {
	if c.stderr {
		if s.onStderr != nil {
			s.onStderr(c.data)
		}
		if s.stderr != nil {
			_, _ = s.stderr.Write(c.data)
		}
	} else {
		if s.onStdout != nil {
			s.onStdout(c.data)
		}
		if s.stdout != nil {
			_, _ = s.stdout.Write(c.data)
		}
	}
	s.progress.Add(1)
}

func (s *executionStreamState) closeDrained() {
	s.drainedOnce.Do(func() { close(s.drained) })
}

// stallWatchdog auto-kills (via the supplied closure) an execution whose
// back-pressure has fully stalled: output is buffered (queuedBytes > 0) but the
// caller's sink has delivered nothing — progress has not advanced — for the
// whole timeout. Resets on any progress, so a slow-but-advancing sink is never
// killed; ignores quiet executions, which buffer nothing. Exits when the stream
// drains (clean end) or after it has fired the kill.
func (s *executionStreamState) stallWatchdog(timeout time.Duration, kill func()) {
	interval := timeout / 4
	if interval < 50*time.Millisecond {
		interval = 50 * time.Millisecond
	}
	t := time.NewTicker(interval)
	defer t.Stop()

	lastProgress := s.progress.Load()
	var stalledSince time.Time // zero = not currently stalled
	for {
		select {
		case <-s.drained:
			return
		case <-t.C:
			now := s.progress.Load()
			progressed := now != lastProgress
			lastProgress = now

			s.qmu.Lock()
			pending := s.queuedBytes
			s.qmu.Unlock()

			// A stall is "output is buffered but nothing got delivered this
			// interval". Time it from when the stall began (not from exec start
			// or last progress), so a sink that goes quiet then stalls still
			// gets the full timeout, and a quiet exec that buffers nothing is
			// never killed.
			if progressed || pending == 0 {
				stalledSince = time.Time{}
				continue
			}
			if stalledSince.IsZero() {
				stalledSince = time.Now()
			} else if time.Since(stalledSince) >= timeout {
				kill()
				return
			}
		}
	}
}

// Execution is a handle to a running command.
type Execution struct {
	handle      *C.CExecutionHandle
	streamState *executionStreamState
	// closing is the parent runtime's close-broadcast channel; Wait/Kill/
	// ResizeTTY select on it so they unblock when Runtime.Close is called
	// while they're parked on their result channel.
	closing <-chan struct{}

	// Stdin writes bytes to the running command's standard input. Closing it
	// signals EOF to the guest process — the analog of `os/exec.Cmd.StdinPipe()`'s
	// io.WriteCloser. After Close(), subsequent Write calls return ErrInvalidState.
	Stdin io.WriteCloser

	closeOnce sync.Once
}

type executionStdin struct {
	execution *Execution
}

// Exec executes a command and returns the buffered result.
func (b *Box) Exec(ctx context.Context, name string, arg ...string) (*ExecResult, error) {
	cmd := b.Command(name, arg...)

	var stdoutBuf, stderrBuf []byte
	execution, err := b.StartExecution(ctx, cmd.Path, cmd.Args, &ExecutionOptions{
		Stdout: &bytesCollector{buf: &stdoutBuf},
		Stderr: &bytesCollector{buf: &stderrBuf},
	})
	if err != nil {
		return nil, err
	}
	defer execution.Close()

	exitCode, err := execution.Wait(ctx)
	if err != nil {
		return nil, err
	}

	return &ExecResult{
		ExitCode: exitCode,
		Stdout:   string(stdoutBuf),
		Stderr:   string(stderrBuf),
	}, nil
}

// StartExecution starts a command and returns a streaming execution handle.
//
// boxlite_box_exec is synchronous on the C side; once it returns, we
// register stream callbacks (which post events into the runtime queue).
func (b *Box) StartExecution(_ context.Context, name string, args []string, opts *ExecutionOptions) (*Execution, error) {
	b.runtime.ensureDrainRunning()

	cCmd := toCString(name)
	defer C.free(unsafe.Pointer(cCmd))

	cArgs, argc := toCStringArray(args)
	defer freeCStringArray(cArgs, argc)

	cfg := ExecutionOptions{}
	if opts != nil {
		cfg = *opts
	}

	envPairs, envCount := envMapToCStringArray(cfg.Env)
	defer freeCStringArray(envPairs, envCount)

	var cWorkdir *C.char
	if cfg.WorkingDir != "" {
		cWorkdir = toCString(cfg.WorkingDir)
		defer C.free(unsafe.Pointer(cWorkdir))
	}

	cCommand := C.BoxliteCommand{
		command:      cCmd,
		args:         cArgs,
		argc:         C.int(argc),
		env_pairs:    envPairs,
		env_count:    C.int(envCount),
		workdir:      cWorkdir,
		user:         nil,
		timeout_secs: C.double(cfg.Timeout.Seconds()),
		tty:          boolToCInt(cfg.TTY),
	}

	var handle *C.CExecutionHandle
	var cerr C.CBoxliteError
	code := C.boxlite_box_exec(b.handle, &cCommand, &handle, &cerr)
	if code != C.Ok {
		return nil, freeError(&cerr)
	}

	// The pause setter throttles this execution's guest producer when the
	// delivery queue fills (end-to-end back-pressure). `handle` is valid for
	// the closure's lifetime: it is only invoked while the stream state is not
	// stopped, and Close frees the handle only after marking it stopped.
	state := newExecutionStreamState(cfg, func(paused bool) {
		var cerr C.CBoxliteError
		C.boxlite_execution_set_stream_paused(handle, boolToCInt(paused), &cerr)
	})
	streamHandle := cgo.NewHandle(state)

	if err := registerExecutionCallbacks(handle, streamHandle); err != nil {
		// On registration failure free the C handle (this aborts any pumps
		// already started) and mark the stream state released so any
		// remaining in-flight events short-circuit. We deliberately leak
		// the cgo.Handle here: deleting it could race with a stream
		// callback that has already entered drain and looked up the value.
		state.markReleased()
		C.boxlite_execution_free(handle)
		return nil, err
	}

	execution := &Execution{
		handle:      handle,
		streamState: state,
		closing:     b.runtime.closing,
	}
	execution.Stdin = &executionStdin{execution: execution}

	if cfg.StreamStallTimeout > 0 {
		go state.stallWatchdog(cfg.StreamStallTimeout, func() {
			_ = execution.Kill(context.Background())
		})
	}
	return execution, nil
}

// registerExecutionCallbacks wires stdout, stderr, and exit on the C side
// using a single shared cgo.Handle so the exit callback (dispatched last)
// can Delete it without racing the stream callbacks.
func registerExecutionCallbacks(handle *C.CExecutionHandle, streamHandle cgo.Handle) error {
	udPtr := handleToPtr(streamHandle)

	var cerr C.CBoxliteError
	if code := C.boxlite_execution_on_stdout(handle, C.cbStdout(), udPtr, &cerr); code != C.Ok {
		return freeError(&cerr)
	}
	if code := C.boxlite_execution_on_stderr(handle, C.cbStderr(), udPtr, &cerr); code != C.Ok {
		return freeError(&cerr)
	}
	if code := C.boxlite_execution_on_exit(handle, C.cbExit(), udPtr, &cerr); code != C.Ok {
		return freeError(&cerr)
	}
	return nil
}

// Write writes bytes to the running command's standard input.
func (e *Execution) Write(p []byte) (int, error) {
	if e.Stdin == nil {
		return 0, &Error{Code: ErrInvalidState, Message: "execution stdin is closed"}
	}
	return e.Stdin.Write(p)
}

// Wait blocks until the process has exited AND every stdout/stderr
// callback for this execution has been dispatched, then returns the
// exit code. Mirrors os/exec.Cmd's Wait for the io.Writer case —
// every BoxLite execution IS the io.Writer case (streams are pushed
// to a user-supplied Writer / callback; there is no StdoutPipe-style
// user-read pipe), so Wait is the single terminal and must guarantee
// output completeness on return.
//
// The post-exit-code drain is non-cancelable by ctx (parity with
// os/exec's awaitGoroutines); only runtime shutdown breaks it, in
// which case the process's exit code is preserved and err is
// overwritten only if the wait had none.
func (e *Execution) Wait(ctx context.Context) (int, error) {
	if e.handle == nil {
		return 0, &Error{Code: ErrInvalidState, Message: "execution is closed"}
	}

	ch := make(chan executionWaitResult, 1)
	h := registerHandleForDispatch(cgo.NewHandle(ch))

	var cerr C.CBoxliteError
	if rc := C.boxlite_execution_wait(e.handle, C.cbExecutionWait(), handleToPtr(h), &cerr); rc != C.Ok {
		deleteHandleForDispatch(h)
		return 0, freeError(&cerr)
	}

	var code int
	var err error
	select {
	case res := <-ch:
		code, err = res.exitCode, res.err
	case <-ctx.Done():
		// ctx cancel before the process reports exit: skip the
		// drain barrier (consistent with os/exec's behavior on
		// Process.Wait cancellation).
		drainAndDelete(ch, h, e.closing)
		return 0, ctx.Err()
	case <-e.closing:
		drainAndDelete(ch, h, e.closing)
		return 0, ErrRuntimeClosed
	}

	// Drain barrier: wait for stream pumps to flush before returning, so
	// the caller's stdout/stderr Writers see every chunk the exec produced.
	// Bounded by stream *progress*, not a fixed wall clock: normally
	// `drained` closes once C's exit pump (which keeps Exit strictly last)
	// fires on_exit. But a SIGKILLed guest can leave a pipe without EOF, so
	// the exit pump never fires and `drained` never closes — keep waiting
	// while output is still arriving (progress advances; a large tail is
	// never cut off), and give up once the stream goes idle. Non-cancelable
	// by ctx (os/exec parity); only runtime shutdown, an idle/stalled
	// stream, or the hard cap breaks it. Preserves the exit code; overwrites
	// err only if the wait had none.
	if closed := waitForStreamDrain(
		e.streamState.drained, e.closing, &e.streamState.progress,
		streamDrainIdleBound, streamDrainHardCap,
	); closed && err == nil {
		err = ErrRuntimeClosed
	}
	return code, err
}

// streamDrainIdleBound is how long Wait's drain barrier tolerates no new
// Stdout/Stderr callback before treating the stream as stalled and
// returning. The drain runs only after the exit code is in hand, so any
// remaining output is buffered and arrives back-to-back; a full window with
// no progress means a guest pipe that will never EOF (a signal-killed
// process). Large enough to absorb scheduler/transport jitter, small enough
// that a timeout-killed exec's Wait returns promptly.
const streamDrainIdleBound = 300 * time.Millisecond

// streamDrainHardCap backstops a pipe that never EOFs yet keeps trickling a
// chunk within every idle window forever. Generous so it never clips a
// genuine high-throughput tail.
const streamDrainHardCap = 30 * time.Second

// waitForStreamDrain blocks until the stream callbacks have flushed
// (`drained` closes), the runtime is closing, the stream goes idle (no
// progress within idleBound — a stuck never-EOF pipe), or hardCap elapses.
// Returns true only when it ended because the runtime is closing, so the
// caller can surface ErrRuntimeClosed. Bounding by progress rather than a
// fixed wall clock keeps a still-delivering tail from being cut off.
func waitForStreamDrain(drained, closing <-chan struct{}, progress *atomic.Uint64, idleBound, hardCap time.Duration) bool {
	idle := time.NewTicker(idleBound)
	defer idle.Stop()
	cap := time.NewTimer(hardCap)
	defer cap.Stop()

	last := progress.Load()
	for {
		select {
		case <-drained:
			return false
		case <-closing:
			return true
		case <-cap.C:
			return false
		case <-idle.C:
			now := progress.Load()
			if now == last {
				return false
			}
			last = now
		}
	}
}

// Kill terminates the running command.
func (e *Execution) Kill(ctx context.Context) error {
	if e.handle == nil {
		return &Error{Code: ErrInvalidState, Message: "execution is closed"}
	}

	ch := make(chan error, 1)
	h := registerHandleForDispatch(cgo.NewHandle(ch))

	var cerr C.CBoxliteError
	code := C.boxlite_execution_kill(e.handle, C.cbExecutionKill(), handleToPtr(h), &cerr)
	if code != C.Ok {
		deleteHandleForDispatch(h)
		return freeError(&cerr)
	}

	select {
	case err := <-ch:
		return err
	case <-ctx.Done():
		abandonAsyncErr(ch, h, e.closing)
		return ctx.Err()
	case <-e.closing:
		abandonAsyncErr(ch, h, e.closing)
		return ErrRuntimeClosed
	}
}

// Signal sends an arbitrary Unix signal (e.g. 1=SIGHUP, 2=SIGINT, 15=SIGTERM)
// to the running command. Use Kill for SIGKILL+evict semantics; Signal is
// for graceful and non-terminal signals that should not tear down the
// per-execution bookkeeping.
//
// `sig` must be in the range 1..=64 (1..=31 standard, 32..=64 RT). Out-of-
// range values are rejected synchronously (no FFI call) so an invalid
// signal can never reach the Rust runtime. Range validation runs BEFORE
// the closed-handle check so callers that pass an invalid signal always
// receive `ErrInvalidArgument` regardless of handle state.
func (e *Execution) Signal(ctx context.Context, sig int) error {
	if sig < 1 || sig > 64 {
		return &Error{
			Code:    ErrInvalidArgument,
			Message: "signal must be in 1..=64",
		}
	}
	if e.handle == nil {
		return &Error{Code: ErrInvalidState, Message: "execution is closed"}
	}

	ch := make(chan error, 1)
	h := registerHandleForDispatch(cgo.NewHandle(ch))

	var cerr C.CBoxliteError
	code := C.boxlite_execution_signal(
		e.handle,
		C.int(sig),
		C.cbExecutionSignal(),
		handleToPtr(h),
		&cerr,
	)
	if code != C.Ok {
		deleteHandleForDispatch(h)
		return freeError(&cerr)
	}

	select {
	case err := <-ch:
		return err
	case <-ctx.Done():
		abandonAsyncErr(ch, h, e.closing)
		return ctx.Err()
	case <-e.closing:
		abandonAsyncErr(ch, h, e.closing)
		return ErrRuntimeClosed
	}
}

// ResizeTTY changes the terminal size for TTY-enabled executions.
func (e *Execution) ResizeTTY(ctx context.Context, rows, cols int) error {
	if e.handle == nil {
		return &Error{Code: ErrInvalidState, Message: "execution is closed"}
	}

	ch := make(chan error, 1)
	h := registerHandleForDispatch(cgo.NewHandle(ch))

	var cerr C.CBoxliteError
	code := C.boxlite_execution_tty_resize(e.handle, C.int(rows), C.int(cols), C.cbExecutionResize(), handleToPtr(h), &cerr)
	if code != C.Ok {
		deleteHandleForDispatch(h)
		return freeError(&cerr)
	}

	select {
	case err := <-ch:
		return err
	case <-ctx.Done():
		abandonAsyncErr(ch, h, e.closing)
		return ctx.Err()
	case <-e.closing:
		abandonAsyncErr(ch, h, e.closing)
		return ErrRuntimeClosed
	}
}

// Close releases the execution handle and signals the stream state that
// no further deliveries are expected. The cgo.Handle backing the stream
// state is intentionally not Deleted here (see executionStreamState for
// rationale).
func (e *Execution) Close() error {
	e.closeOnce.Do(func() {
		if e.streamState != nil {
			e.streamState.markReleased()
		}
		if e.handle != nil {
			C.boxlite_execution_free(e.handle)
			e.handle = nil
		}
	})
	return nil
}

func (s *executionStdin) Write(p []byte) (int, error) {
	if len(p) == 0 {
		return 0, nil
	}
	if s.execution == nil || s.execution.handle == nil {
		return 0, &Error{Code: ErrInvalidState, Message: "execution is closed"}
	}

	var cerr C.CBoxliteError
	code := C.boxlite_execution_stdin_write(
		s.execution.handle,
		(*C.uint8_t)(unsafe.Pointer(&p[0])),
		C.size_t(len(p)),
		&cerr,
	)
	if code != C.Ok {
		return 0, freeError(&cerr)
	}
	return len(p), nil
}

// Close signals EOF to the guest process's stdin. Idempotent: a second call
// is a no-op. Mirrors `os/exec.Cmd.StdinPipe()`'s io.WriteCloser contract.
func (s *executionStdin) Close() error {
	if s.execution == nil || s.execution.handle == nil {
		return nil
	}
	var cerr C.CBoxliteError
	code := C.boxlite_execution_stdin_close(s.execution.handle, &cerr)
	if code != C.Ok {
		return freeError(&cerr)
	}
	return nil
}

// Command creates a Cmd for streaming execution, mirroring os/exec.Cmd.
func (b *Box) Command(name string, arg ...string) *Cmd {
	return &Cmd{
		Path: name,
		Args: arg,
		box:  b,
	}
}

// Cmd represents a command to execute inside a box.
// It mirrors the os/exec.Cmd pattern: callers configure Env/Dir/Timeout
// on the struct before invoking Run/Output/CombinedOutput. Zero-value
// fields inherit the container default — same semantics as os/exec.Cmd
// with the addition of Timeout, which bounds wall-clock lifetime
// (zero = unbounded, matching the C-FFI's `timeout_secs <= 0` convention).
type Cmd struct {
	Path   string
	Args   []string
	Stdout io.Writer
	Stderr io.Writer
	// Env is the environment of the executed process. nil/empty inherits
	// the container default.
	Env map[string]string
	// Dir is the working directory inside the container. Empty inherits
	// the container default. Named Dir (not WorkingDir) to match os/exec.
	Dir string
	// Timeout bounds wall-clock lifetime. Zero = no timeout.
	Timeout time.Duration

	box      *Box
	exitCode int
	done     bool
}

// Run executes the command. If Stdout/Stderr are set, output is streamed to them.
func (c *Cmd) Run(ctx context.Context) error {
	execution, err := c.box.StartExecution(ctx, c.Path, c.Args, &ExecutionOptions{
		Stdout:     c.Stdout,
		Stderr:     c.Stderr,
		Env:        c.Env,
		WorkingDir: c.Dir,
		Timeout:    c.Timeout,
	})
	if err != nil {
		return err
	}
	defer execution.Close()

	exitCode, err := execution.Wait(ctx)
	if err != nil {
		return err
	}

	c.exitCode = exitCode
	c.done = true
	return nil
}

// Output runs the command and returns its standard output.
func (c *Cmd) Output(ctx context.Context) ([]byte, error) {
	var buf bytes.Buffer
	c.Stdout = &buf
	err := c.Run(ctx)
	return buf.Bytes(), err
}

// CombinedOutput runs the command and returns combined stdout and stderr.
func (c *Cmd) CombinedOutput(ctx context.Context) ([]byte, error) {
	var buf bytes.Buffer
	c.Stdout = &buf
	c.Stderr = &buf
	err := c.Run(ctx)
	return buf.Bytes(), err
}

// ExitCode returns the exit code of the command. Only valid after Run completes.
func (c *Cmd) ExitCode() int {
	return c.exitCode
}

type bytesCollector struct {
	buf *[]byte
}

func (w *bytesCollector) Write(p []byte) (int, error) {
	*w.buf = append(*w.buf, p...)
	return len(p), nil
}
