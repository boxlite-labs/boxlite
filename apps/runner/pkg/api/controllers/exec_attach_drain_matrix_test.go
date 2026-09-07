// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 BoxLite AI

package controllers

import (
	"encoding/json"
	"strings"
	"testing"
	"time"

	"github.com/gorilla/websocket"
)

// drainResult is what attachDrainOutcome observed driving one /attach session.
type drainResult struct {
	exitSeen    bool
	stdoutBytes int
	stderrBytes int
	trailing    int // stream bytes that arrived AFTER the exit frame (ordering)
}

// attachDrainOutcome drives the real BoxliteExecAttach handler over a websocket
// with the given stub producer, then reports what the client observed. producer
// performs the stream writes, EOFs, and fires Done. tty selects TTY mode (the
// handler then runs only the stdout pump). Reads stop once the exit frame and
// any trailing frames are seen, or after a hang deadline.
func attachDrainOutcome(t *testing.T, tty bool, exitCode int, producer func(s *stubAttachExec)) drainResult {
	t.Helper()
	const stdoutByte, stderrByte = 0x01, 0x02

	stub := newStubAttachExec()
	stub.exitCode = exitCode
	stub.tty = tty
	cleanup := withStubExec(t, "exec-matrix", stub)
	defer cleanup()
	srv := newAttachServer(t)
	defer srv.Close()
	conn, _, err := dialAttach(t, srv, "exec-matrix")
	if err != nil {
		t.Fatalf("dial: %v", err)
	}
	defer conn.Close()

	go producer(stub)

	var r drainResult
	seenExit := false
	count := func(payload []byte) {
		if len(payload) < 1 {
			return
		}
		n := len(payload) - 1
		switch payload[0] {
		case stdoutByte:
			r.stdoutBytes += n
		case stderrByte:
			r.stderrBytes += n
		}
		if seenExit {
			r.trailing += n
		}
	}

	for {
		_ = conn.SetReadDeadline(time.Now().Add(5 * time.Second))
		mt, payload, err := conn.ReadMessage()
		if err != nil {
			return r // deadline/close without exit => HANG (exitSeen stays false)
		}
		switch mt {
		case websocket.BinaryMessage:
			count(payload)
		case websocket.TextMessage:
			var ev map[string]any
			if json.Unmarshal(payload, &ev) == nil && ev["type"] == "exit" {
				r.exitSeen = true
				seenExit = true
				// Drain any trailing frames briefly to catch ordering bugs.
				_ = conn.SetReadDeadline(time.Now().Add(400 * time.Millisecond))
				for {
					mt2, p2, e2 := conn.ReadMessage()
					if e2 != nil {
						return r
					}
					if mt2 == websocket.BinaryMessage {
						count(p2)
					}
				}
			}
		}
	}
}

// TestAttachDrainMatrix locks down PR #812's runner-side drain across the
// (output x stream-termination) matrix: clean exits flush all output with the
// exit frame strictly last; a SIGKILLed never-EOF pipe gives up after the idle
// window and still sends exit instead of hanging.
func TestAttachDrainMatrix(t *testing.T) {
	big := strings.Repeat("a", 64*1024) // 16 chunks < 256 channel buffer: lossless

	cases := []struct {
		name       string
		tty        bool
		wantStdout int
		wantStderr int
		producer   func(s *stubAttachExec)
	}{
		{"clean-none", false, 0, 0, func(s *stubAttachExec) {
			s.stdoutW.Close()
			close(s.done)
		}},
		{"clean-short", false, 2, 0, func(s *stubAttachExec) {
			s.stdoutW.Write([]byte("hi"))
			s.stdoutW.Close()
			close(s.done)
		}},
		{"clean-long-64k", false, len(big), 0, func(s *stubAttachExec) {
			s.stdoutW.Write([]byte(big))
			s.stdoutW.Close()
			close(s.done)
		}},
		{"stderr-channel", false, 3, 7, func(s *stubAttachExec) {
			s.stdoutW.Write([]byte("OUT"))
			s.stderrW.Write([]byte("ERRLINE"))
			s.stdoutW.Close()
			s.stderrW.Close()
			close(s.done)
		}},
		{"tty-merged", true, 9, 0, func(s *stubAttachExec) {
			s.stdoutW.Write([]byte("hello-tty"))
			s.stdoutW.Close()
			s.stderrW.Close()
			close(s.done)
		}},
		{"stuck-never-eof", false, 2, 0, func(s *stubAttachExec) {
			// SIGKILLed guest: exit is known (Done) but stdout never EOFs.
			s.stdoutW.Write([]byte("hi"))
			close(s.done)
			// deliberately never close stdoutW
		}},
	}

	for _, c := range cases {
		c := c
		t.Run(c.name, func(t *testing.T) {
			r := attachDrainOutcome(t, c.tty, 7, c.producer)
			if !r.exitSeen {
				t.Fatalf("HANG: no exit frame within deadline (%s)", c.name)
			}
			if r.trailing != 0 {
				t.Fatalf("ORDERING: %d stream bytes arrived AFTER the exit frame", r.trailing)
			}
			if r.stdoutBytes != c.wantStdout {
				t.Fatalf("stdout: got %d want %d", r.stdoutBytes, c.wantStdout)
			}
			if r.stderrBytes != c.wantStderr {
				t.Fatalf("stderr: got %d want %d", r.stderrBytes, c.wantStderr)
			}
		})
	}
}
