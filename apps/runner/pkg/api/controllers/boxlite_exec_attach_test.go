// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 BoxLite AI

package controllers

import (
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"net/http/httptest"
	"strings"
	"sync"
	"sync/atomic"
	"testing"
	"time"

	"github.com/gin-gonic/gin"
	"github.com/gorilla/websocket"
)

// stubAttachExec is a substitutable attachExec for unit-testing the
// /attach handler without a live VM. Behavior is recorded into atomics
// and channels so assertions can read them after the handler returns.
type stubAttachExec struct {
	stdoutR    *io.PipeReader
	stdoutW    *io.PipeWriter
	stderrR    *io.PipeReader
	stderrW    *io.PipeWriter
	stdinR     *io.PipeReader
	stdinW     *io.PipeWriter
	done       chan struct{}
	exitCode   int
	tty        bool
	connected  atomic.Bool
	disconnect atomic.Int32

	mu             sync.Mutex
	resizeRows     int
	resizeCols     int
	resizeCalls    atomic.Int32
	signalCalls    atomic.Int32
	signaledWith   []int
	stdinClosed    atomic.Bool
	signalErrToRet error

	// allowSecondAttach: when false, MarkConnected returns false on second call.
	allowSecondAttach bool

	// Subscriber fan-out: mirrors the production broadcaster so tests
	// exercise the same channel-based path the real /attach handler uses.
	subMu        sync.Mutex
	subs         []stubSubscriber
	broadcasting bool
}

type stubSubscriber struct {
	stdout chan []byte
	stderr chan []byte
}

func newStubAttachExec() *stubAttachExec {
	stdoutR, stdoutW := io.Pipe()
	stderrR, stderrW := io.Pipe()
	stdinR, stdinW := io.Pipe()
	return &stubAttachExec{
		stdoutR: stdoutR, stdoutW: stdoutW,
		stderrR: stderrR, stderrW: stderrW,
		stdinR: stdinR, stdinW: stdinW,
		done: make(chan struct{}),
	}
}

// Subscribe spawns a stdout/stderr pump that fan-outs to the returned
// channels. Calling Subscribe more than once is supported (mirrors the
// production fan-out broadcaster); each subscriber gets its own channels and
// sees all subsequent bytes. The pump goroutines exit when the stdout/stderr
// pipes EOF or when the subscriber is cancelled.
func (s *stubAttachExec) Subscribe(bufSize int) (stdout, stderr <-chan []byte, cancel func()) {
	if bufSize <= 0 {
		bufSize = 256
	}
	outCh := make(chan []byte, bufSize)
	errCh := make(chan []byte, bufSize)

	s.subMu.Lock()
	s.subs = append(s.subs, stubSubscriber{stdout: outCh, stderr: errCh})
	if !s.broadcasting {
		s.broadcasting = true
		go s.broadcastPipe(s.stdoutR, true)
		go s.broadcastPipe(s.stderrR, false)
	}
	s.subMu.Unlock()

	once := sync.Once{}
	cancel = func() {
		once.Do(func() {
			s.subMu.Lock()
			for i, sub := range s.subs {
				if sub.stdout == outCh {
					s.subs = append(s.subs[:i], s.subs[i+1:]...)
					break
				}
			}
			s.subMu.Unlock()
			close(outCh)
			close(errCh)
		})
	}
	return outCh, errCh, cancel
}

// broadcastPipe is the test-side analog of ManagedExec.broadcastPipe. Same
// invariants: single reader per pipe, non-blocking fan-out, drop on slow
// subscriber.
func (s *stubAttachExec) broadcastPipe(r *io.PipeReader, isStdout bool) {
	buf := make([]byte, 4096)
	for {
		n, err := r.Read(buf)
		if n > 0 {
			chunk := make([]byte, n)
			copy(chunk, buf[:n])
			s.subMu.Lock()
			for _, sub := range s.subs {
				ch := sub.stdout
				if !isStdout {
					ch = sub.stderr
				}
				select {
				case ch <- chunk:
				default:
				}
			}
			s.subMu.Unlock()
		}
		if err != nil {
			return
		}
	}
}

func (s *stubAttachExec) WriteStdin(data []byte) (int, error) {
	if s.stdinW == nil {
		return 0, fmt.Errorf("stdin is closed")
	}
	return s.stdinW.Write(data)
}
func (s *stubAttachExec) DoneCh() <-chan struct{} { return s.done }
func (s *stubAttachExec) ExitCodeValue() int      { return s.exitCode }
func (s *stubAttachExec) IsTTY() bool             { return s.tty }
func (s *stubAttachExec) Resize(rows, cols int) error {
	s.mu.Lock()
	s.resizeRows = rows
	s.resizeCols = cols
	s.mu.Unlock()
	s.resizeCalls.Add(1)
	return nil
}
func (s *stubAttachExec) Signal(sig int) error {
	s.mu.Lock()
	s.signaledWith = append(s.signaledWith, sig)
	s.mu.Unlock()
	s.signalCalls.Add(1)
	return s.signalErrToRet
}
func (s *stubAttachExec) CloseStdin() error {
	s.stdinClosed.Store(true)
	return s.stdinW.Close()
}

func (s *stubAttachExec) MarkConnected() bool {
	if s.connected.CompareAndSwap(false, true) {
		return true
	}
	return s.allowSecondAttach
}

func (s *stubAttachExec) MarkDisconnected() {
	s.connected.Store(false)
	s.disconnect.Add(1)
}

// withStubExec installs target as the resolveAttachExec result for execId
// for the duration of the test. Returns a cleanup func.
func withStubExec(t *testing.T, execId string, target attachExec) func() {
	t.Helper()
	prev := resolveAttachExec
	resolveAttachExec = func(id, _ string) (attachExec, bool) {
		if id == execId {
			return target, true
		}
		return prev(id, "")
	}
	return func() { resolveAttachExec = prev }
}

// newAttachServer returns an httptest.Server that routes
// /v1/boxes/:boxId/executions/:execId/attach to BoxliteExecAttach.
func newAttachServer(t *testing.T) *httptest.Server {
	t.Helper()
	gin.SetMode(gin.TestMode)
	r := gin.New()
	r.GET("/v1/boxes/:boxId/executions/:execId/attach", BoxliteExecAttach)
	return httptest.NewServer(r)
}

// dialAttach opens a WS connection to /v1/boxes/box/executions/<execId>/attach
// on the given httptest server.
func dialAttach(t *testing.T, srv *httptest.Server, execId string) (*websocket.Conn, *http.Response, error) {
	t.Helper()
	wsURL := strings.Replace(srv.URL, "http://", "ws://", 1) +
		"/v1/boxes/box/executions/" + execId + "/attach"
	dialer := *websocket.DefaultDialer
	dialer.HandshakeTimeout = 5 * time.Second
	return dialer.Dial(wsURL, nil)
}

// readNextNonPongFrame reads frames, ignoring control pongs. Returns the
// first data frame.
func readNextDataFrame(t *testing.T, conn *websocket.Conn, deadline time.Duration) (int, []byte, error) {
	t.Helper()
	_ = conn.SetReadDeadline(time.Now().Add(deadline))
	mt, payload, err := conn.ReadMessage()
	return mt, payload, err
}

func TestBoxliteExecAttach_StdinAndExit(t *testing.T) {
	stub := newStubAttachExec()
	stub.exitCode = 42
	cleanup := withStubExec(t, "exec-1", stub)
	defer cleanup()

	srv := newAttachServer(t)
	defer srv.Close()

	conn, resp, err := dialAttach(t, srv, "exec-1")
	if err != nil {
		t.Fatalf("dial attach: %v (resp=%v)", err, resp)
	}
	defer conn.Close()

	// Goroutine: drain just enough stdin to confirm the pump delivered.
	// io.ReadAll would deadlock — the handler never closes stdinW.
	stdinDone := make(chan []byte, 1)
	go func() {
		buf := make([]byte, 64)
		n, _ := stub.stdinR.Read(buf)
		stdinDone <- buf[:n]
	}()

	// Send binary stdin frame.
	if err := conn.WriteMessage(websocket.BinaryMessage, []byte("ls\n")); err != nil {
		t.Fatalf("write stdin: %v", err)
	}

	// Push some stdout, expect to read back a binary frame with 0x01 prefix.
	go func() {
		_, _ = stub.stdoutW.Write([]byte("hello"))
	}()

	mt, payload, err := readNextDataFrame(t, conn, 2*time.Second)
	if err != nil {
		t.Fatalf("read stdout frame: %v", err)
	}
	if mt != websocket.BinaryMessage {
		t.Fatalf("expected BinaryMessage, got %d", mt)
	}
	if len(payload) < 1 || payload[0] != 0x01 {
		t.Fatalf("expected 0x01 stdout prefix, got payload=% x", payload)
	}
	if string(payload[1:]) != "hello" {
		t.Fatalf("expected payload 'hello', got %q", string(payload[1:]))
	}

	// Fire Done. Handler should emit text exit frame then close.
	stub.stdoutW.Close()
	stub.stderrW.Close()
	close(stub.done)

	// Read frames until we see exit JSON or close.
	deadline := time.Now().Add(3 * time.Second)
	gotExit := false
	for time.Now().Before(deadline) {
		_ = conn.SetReadDeadline(time.Now().Add(2 * time.Second))
		mt, payload, err := conn.ReadMessage()
		if err != nil {
			// Close error is acceptable AFTER we've seen exit.
			if gotExit {
				break
			}
			if ce, ok := err.(*websocket.CloseError); ok {
				t.Fatalf("got close before exit frame: %v", ce)
			}
			t.Fatalf("unexpected read err: %v", err)
		}
		if mt == websocket.TextMessage {
			var ev map[string]interface{}
			if err := json.Unmarshal(payload, &ev); err != nil {
				t.Fatalf("bad exit json: %v", err)
			}
			if ev["type"] == "exit" {
				if got, want := int(ev["exit_code"].(float64)), 42; got != want {
					t.Fatalf("expected exit_code %d, got %d", want, got)
				}
				gotExit = true
				continue
			}
		}
	}
	if !gotExit {
		t.Fatal("did not receive exit text frame within deadline")
	}

	// Confirm stdin was forwarded.
	select {
	case got := <-stdinDone:
		if string(got) != "ls\n" {
			t.Fatalf("expected stdin %q, got %q", "ls\n", got)
		}
	case <-time.After(2 * time.Second):
		t.Fatal("stdin pump did not deliver bytes")
	}

	// Confirm disconnect was marked. The handler runs MarkDisconnected
	// in a deferred block after wg.Wait, so allow a brief settle window.
	deadline = time.Now().Add(2 * time.Second)
	for time.Now().Before(deadline) && stub.disconnect.Load() == 0 {
		time.Sleep(10 * time.Millisecond)
	}
	if stub.disconnect.Load() == 0 {
		t.Fatal("expected MarkDisconnected to be called")
	}
}

func TestBoxliteExecAttach_SingleAttach409(t *testing.T) {
	stub := newStubAttachExec()
	cleanup := withStubExec(t, "exec-busy", stub)
	defer cleanup()

	srv := newAttachServer(t)
	defer srv.Close()

	// First attach succeeds.
	conn1, resp1, err := dialAttach(t, srv, "exec-busy")
	if err != nil {
		t.Fatalf("first dial failed: %v (resp=%v)", err, resp1)
	}
	defer conn1.Close()

	// Second attach must return 409 BEFORE upgrade.
	wsURL := strings.Replace(srv.URL, "http://", "ws://", 1) +
		"/v1/boxes/box/executions/exec-busy/attach"
	dialer := *websocket.DefaultDialer
	dialer.HandshakeTimeout = 2 * time.Second
	conn2, resp2, err := dialer.Dial(wsURL, nil)
	if err == nil {
		conn2.Close()
		t.Fatal("expected second dial to fail with 409")
	}
	if resp2 == nil {
		t.Fatalf("expected http response on second dial, got nil (err=%v)", err)
	}
	if resp2.StatusCode != http.StatusConflict {
		t.Fatalf("expected status 409, got %d", resp2.StatusCode)
	}
}

func TestBoxliteExecAttach_ResizeFrame(t *testing.T) {
	stub := newStubAttachExec()
	stub.tty = true
	cleanup := withStubExec(t, "exec-resize", stub)
	defer cleanup()

	srv := newAttachServer(t)
	defer srv.Close()

	conn, _, err := dialAttach(t, srv, "exec-resize")
	if err != nil {
		t.Fatalf("dial: %v", err)
	}
	defer conn.Close()

	resizeFrame := `{"type":"resize","cols":120,"rows":40}`
	if err := conn.WriteMessage(websocket.TextMessage, []byte(resizeFrame)); err != nil {
		t.Fatalf("write resize: %v", err)
	}

	// Wait until the resize call is observed.
	deadline := time.Now().Add(2 * time.Second)
	for time.Now().Before(deadline) {
		if stub.resizeCalls.Load() > 0 {
			break
		}
		time.Sleep(10 * time.Millisecond)
	}
	if stub.resizeCalls.Load() == 0 {
		t.Fatal("Resize was not called")
	}
	stub.mu.Lock()
	rows, cols := stub.resizeRows, stub.resizeCols
	stub.mu.Unlock()
	if rows != 40 || cols != 120 {
		t.Fatalf("expected rows=40 cols=120, got rows=%d cols=%d", rows, cols)
	}
}

func TestBoxliteExecAttach_StdinEofClosesStdin(t *testing.T) {
	stub := newStubAttachExec()
	cleanup := withStubExec(t, "exec-eof", stub)
	defer cleanup()

	srv := newAttachServer(t)
	defer srv.Close()

	conn, _, err := dialAttach(t, srv, "exec-eof")
	if err != nil {
		t.Fatalf("dial: %v", err)
	}
	defer conn.Close()

	// Drain stdin in background — we need the writer to actually flush
	// before the close is observable, but the pipe blocks until read.
	stdinDone := make(chan struct{})
	go func() {
		_, _ = io.Copy(io.Discard, stub.stdinR)
		close(stdinDone)
	}()

	if err := conn.WriteMessage(websocket.TextMessage, []byte(`{"type":"stdin_eof"}`)); err != nil {
		t.Fatalf("write eof: %v", err)
	}

	// stdin pipe closes once handler invokes CloseStdin.
	select {
	case <-stdinDone:
	case <-time.After(2 * time.Second):
		t.Fatal("stdin pipe did not close after stdin_eof frame")
	}

	if !stub.stdinClosed.Load() {
		t.Fatal("CloseStdin was not called")
	}
}

func TestBoxliteExecAttach_KeepalivePing(t *testing.T) {
	// Speed the keepalive interval up.
	restore := setKeepaliveIntervalForTest(50 * time.Millisecond)
	defer restore()

	stub := newStubAttachExec()
	cleanup := withStubExec(t, "exec-ping", stub)
	defer cleanup()

	srv := newAttachServer(t)
	defer srv.Close()

	conn, _, err := dialAttach(t, srv, "exec-ping")
	if err != nil {
		t.Fatalf("dial: %v", err)
	}
	defer conn.Close()

	pingSeen := make(chan struct{}, 1)
	conn.SetPingHandler(func(appData string) error {
		select {
		case pingSeen <- struct{}{}:
		default:
		}
		// Standard pong reply.
		return conn.WriteControl(websocket.PongMessage, []byte(appData), time.Now().Add(time.Second))
	})

	// We need to drive ReadMessage to dispatch the control frame handler.
	readErr := make(chan error, 1)
	go func() {
		_ = conn.SetReadDeadline(time.Now().Add(2 * time.Second))
		_, _, err := conn.ReadMessage()
		readErr <- err
	}()

	select {
	case <-pingSeen:
		// good
	case err := <-readErr:
		t.Fatalf("read returned before ping observed: %v", err)
	case <-time.After(1 * time.Second):
		t.Fatal("did not observe a server ping within 1s (interval=50ms)")
	}
}

func TestBoxliteExecAttach_PongRefreshesBoxActivity(t *testing.T) {
	restore := setKeepaliveIntervalForTest(50 * time.Millisecond)
	defer restore()

	activityRequests := make(chan string, 1)
	api := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Method != http.MethodPost || r.URL.Path != "/box/box/last-activity" {
			http.NotFound(w, r)
			return
		}
		activityRequests <- r.Header.Get("Authorization")
		w.WriteHeader(http.StatusNoContent)
	}))
	defer api.Close()

	t.Setenv("BOXLITE_API_URL", api.URL)
	t.Setenv("BOXLITE_RUNNER_TOKEN", "runner-token")

	stub := newStubAttachExec()
	cleanup := withStubExec(t, "exec-activity", stub)
	defer cleanup()

	srv := newAttachServer(t)
	defer srv.Close()

	conn, _, err := dialAttach(t, srv, "exec-activity")
	if err != nil {
		t.Fatalf("dial: %v", err)
	}
	defer conn.Close()

	conn.SetPingHandler(func(appData string) error {
		return conn.WriteControl(websocket.PongMessage, []byte(appData), time.Now().Add(time.Second))
	})

	readErr := make(chan error, 1)
	go func() {
		_ = conn.SetReadDeadline(time.Now().Add(2 * time.Second))
		_, _, err := conn.ReadMessage()
		readErr <- err
	}()

	select {
	case auth := <-activityRequests:
		if auth != "Bearer runner-token" {
			t.Fatalf("expected runner auth header, got %q", auth)
		}
	case err := <-readErr:
		t.Fatalf("client read returned before activity refresh: %v", err)
	case <-time.After(2 * time.Second):
		t.Fatal("runner did not refresh box activity after websocket pong")
	}
}

func TestValidateInbandSignal_RejectsKillStopCont(t *testing.T) {
	for _, sig := range []int{9, 17, 18, 19, 23} {
		if err := validateInbandSignal(sig); err == nil {
			t.Fatalf("expected sig %d to be rejected", sig)
		}
	}
	for _, sig := range []int{1, 2, 3, 6, 10, 12, 15, 28} {
		if err := validateInbandSignal(sig); err != nil {
			t.Fatalf("expected sig %d allowed, got err: %v", sig, err)
		}
	}
}

func TestBoxliteExecAttach_SignalFrameRejectedSendsError(t *testing.T) {
	stub := newStubAttachExec()
	cleanup := withStubExec(t, "exec-sig-bad", stub)
	defer cleanup()

	srv := newAttachServer(t)
	defer srv.Close()

	conn, _, err := dialAttach(t, srv, "exec-sig-bad")
	if err != nil {
		t.Fatalf("dial: %v", err)
	}
	defer conn.Close()

	// 9 (KILL) is rejected by the in-band validator.
	if err := conn.WriteMessage(websocket.TextMessage, []byte(`{"type":"signal","sig":9}`)); err != nil {
		t.Fatalf("write signal: %v", err)
	}

	_ = conn.SetReadDeadline(time.Now().Add(2 * time.Second))
	mt, payload, err := conn.ReadMessage()
	if err != nil {
		t.Fatalf("read err: %v", err)
	}
	if mt != websocket.TextMessage {
		t.Fatalf("expected text frame, got mt=%d", mt)
	}
	var ev map[string]interface{}
	if err := json.Unmarshal(payload, &ev); err != nil {
		t.Fatalf("bad json: %v", err)
	}
	if ev["type"] != "error" {
		t.Fatalf("expected type=error, got %v", ev["type"])
	}
	if stub.signalCalls.Load() != 0 {
		t.Fatal("Signal must NOT be called for rejected sig")
	}
}

func TestBoxliteExecAttach_SignalFrameAllowedDelivered(t *testing.T) {
	stub := newStubAttachExec()
	cleanup := withStubExec(t, "exec-sig-ok", stub)
	defer cleanup()

	srv := newAttachServer(t)
	defer srv.Close()

	conn, _, err := dialAttach(t, srv, "exec-sig-ok")
	if err != nil {
		t.Fatalf("dial: %v", err)
	}
	defer conn.Close()

	// 2 (INT) is allowed.
	if err := conn.WriteMessage(websocket.TextMessage, []byte(`{"type":"signal","sig":2}`)); err != nil {
		t.Fatalf("write signal: %v", err)
	}

	deadline := time.Now().Add(2 * time.Second)
	for time.Now().Before(deadline) {
		if stub.signalCalls.Load() > 0 {
			break
		}
		time.Sleep(10 * time.Millisecond)
	}
	if stub.signalCalls.Load() == 0 {
		t.Fatal("Signal was not delivered for allowed sig")
	}
	stub.mu.Lock()
	got := append([]int(nil), stub.signaledWith...)
	stub.mu.Unlock()
	if len(got) != 1 || got[0] != 2 {
		t.Fatalf("expected delivered sig=[2], got %v", got)
	}
}

// A client that never pongs (suppressed via SetPingHandler that swallows
// pings) must be evicted within ~3 keepalive intervals so the single-attach
// slot is released. Without pong-based liveness the server's tiny ping write
// fits in the kernel send buffer indefinitely on a half-open TCP and the slot
// is held for 10-15 minutes.
func TestBoxliteExecAttach_PongTimeoutEvictsDeadClient(t *testing.T) {
	restore := setKeepaliveIntervalForTest(50 * time.Millisecond)
	defer restore()

	stub := newStubAttachExec()
	cleanup := withStubExec(t, "exec-deadpong", stub)
	defer cleanup()

	srv := newAttachServer(t)
	defer srv.Close()

	conn, _, err := dialAttach(t, srv, "exec-deadpong")
	if err != nil {
		t.Fatalf("dial: %v", err)
	}
	defer conn.Close()

	// Suppress the client-side auto-pong so the server never sees a Pong.
	// Returning nil from the handler tells gorilla to drop the ping silently.
	conn.SetPingHandler(func(string) error { return nil })

	// Drive ReadMessage so gorilla's internal control-frame dispatch runs
	// (pings would otherwise queue). Read frames until the server gives up.
	readErr := make(chan error, 1)
	go func() {
		_ = conn.SetReadDeadline(time.Now().Add(2 * time.Second))
		for {
			if _, _, err := conn.ReadMessage(); err != nil {
				readErr <- err
				return
			}
		}
	}()

	// pongWait = 3 * 50ms = 150ms; expect MarkDisconnected within ~500ms.
	deadline := time.After(1500 * time.Millisecond)
	for {
		if stub.disconnect.Load() > 0 {
			return // success: handler tore down and released the slot
		}
		select {
		case <-deadline:
			t.Fatalf("server did not detect dead client within 1.5s (disconnect=%d, readErr=%v)",
				stub.disconnect.Load(), tryReceive(readErr))
		case <-time.After(20 * time.Millisecond):
		}
	}
}

// tryReceive returns the channel value if available, otherwise nil — used to
// avoid blocking when surfacing context in a failure message.
func tryReceive(ch <-chan error) error {
	select {
	case v := <-ch:
		return v
	default:
		return nil
	}
}

// Sanity guard: handler returns 404 if exec id is unknown (resolver returns false).
func TestBoxliteExecAttach_NotFound(t *testing.T) {
	prev := resolveAttachExec
	resolveAttachExec = func(id, _ string) (attachExec, bool) { return nil, false }
	defer func() { resolveAttachExec = prev }()

	srv := newAttachServer(t)
	defer srv.Close()

	wsURL := strings.Replace(srv.URL, "http://", "ws://", 1) +
		"/v1/boxes/box/executions/missing/attach"
	dialer := *websocket.DefaultDialer
	dialer.HandshakeTimeout = 2 * time.Second
	conn, resp, err := dialer.Dial(wsURL, nil)
	if err == nil {
		conn.Close()
		t.Fatal("expected dial to fail for unknown exec")
	}
	if resp == nil || resp.StatusCode != http.StatusNotFound {
		t.Fatalf("expected 404, got %v err=%v", resp, err)
	}
}
