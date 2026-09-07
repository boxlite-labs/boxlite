// Copyright 2026 BoxLite AI
// SPDX-License-Identifier: AGPL-3.0

package controllers

import (
	"context"
	"io"
	"log/slog"
	"net/http"
	"net/http/httptest"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/gorilla/websocket"
)

const testTerminalKeepaliveInterval = 50 * time.Millisecond

func TestIsTerminalToolboxPath(t *testing.T) {
	tests := []struct {
		path string
		want bool
	}{
		{"", true},
		{"/", true},
		{"proxy/22222", true},
		{"/proxy/22222", true},
		{"/proxy/22222/", true},
		{"/proxy/22222/vnc.html", true},
		{"/proxy/6080/", false},
		{"/computeruse/status", false},
		{"/process/execute", false},
	}

	for _, tt := range tests {
		t.Run(tt.path, func(t *testing.T) {
			if got := isTerminalToolboxPath(tt.path); got != tt.want {
				t.Fatalf("isTerminalToolboxPath(%q) = %v, want %v", tt.path, got, tt.want)
			}
		})
	}
}

func TestTerminalPongRefreshesBoxActivity(t *testing.T) {
	touches := make(chan struct{}, 1)
	toucher := &boxActivityToucher{
		boxID:       "box",
		minInterval: time.Millisecond,
		update: func(context.Context, string) error {
			select {
			case touches <- struct{}{}:
			default:
			}
			return nil
		},
		logger: slog.New(slog.NewTextHandler(io.Discard, nil)),
	}

	srv := newTerminalKeepaliveServer(t, toucher, testTerminalKeepaliveInterval)
	defer srv.Close()

	conn, _, err := websocket.DefaultDialer.Dial(strings.Replace(srv.URL, "http://", "ws://", 1), nil)
	if err != nil {
		t.Fatalf("dial terminal ws: %v", err)
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
	case <-touches:
	case err := <-readErr:
		t.Fatalf("client read returned before terminal activity refresh: %v", err)
	case <-time.After(2 * time.Second):
		t.Fatal("terminal pong did not refresh box activity")
	}
}

func TestTerminalPongTimeoutEndsReadLoop(t *testing.T) {
	handlerErr := make(chan error, 1)
	srv := newTerminalKeepaliveServerWithReadResult(t, nil, handlerErr, testTerminalKeepaliveInterval)
	defer srv.Close()

	conn, _, err := websocket.DefaultDialer.Dial(strings.Replace(srv.URL, "http://", "ws://", 1), nil)
	if err != nil {
		t.Fatalf("dial terminal ws: %v", err)
	}
	defer conn.Close()

	conn.SetPingHandler(func(string) error { return nil })

	go func() {
		_ = conn.SetReadDeadline(time.Now().Add(2 * time.Second))
		for {
			if _, _, err := conn.ReadMessage(); err != nil {
				return
			}
		}
	}()

	select {
	case err := <-handlerErr:
		if err == nil {
			t.Fatal("expected terminal read loop to end with a timeout error")
		}
	case <-time.After(1500 * time.Millisecond):
		t.Fatal("terminal read loop did not end after missing pongs")
	}
}

func newTerminalKeepaliveServer(t *testing.T, toucher *boxActivityToucher, interval time.Duration) *httptest.Server {
	t.Helper()
	return newTerminalKeepaliveServerWithReadResult(t, toucher, make(chan error, 1), interval)
}

func newTerminalKeepaliveServerWithReadResult(t *testing.T, toucher *boxActivityToucher, handlerErr chan<- error, interval time.Duration) *httptest.Server {
	t.Helper()
	logger := slog.New(slog.NewTextHandler(io.Discard, nil))
	return httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		conn, err := upgrader.Upgrade(w, r, nil)
		if err != nil {
			handlerErr <- err
			return
		}
		defer conn.Close()

		ctx, cancel := context.WithCancel(r.Context())
		defer cancel()

		var writeMu sync.Mutex
		configurePongLiveness(ctx, conn, 3*interval, toucher.Touch)
		go runWebSocketKeepalive(ctx, conn, &writeMu, interval, terminalWriteDeadline, func(err error) {
			logger.Debug("terminal keepalive ping failed", "error", err)
		})

		for {
			if _, _, err := conn.ReadMessage(); err != nil {
				handlerErr <- err
				return
			}
		}
	}))
}
