// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 BoxLite AI

package controllers

import (
	"context"
	"fmt"
	"sync"
	"time"

	"github.com/gorilla/websocket"
)

type pongActivityFunc func(context.Context)

func configurePongLiveness(ctx context.Context, conn *websocket.Conn, pongWait time.Duration, touch pongActivityFunc) {
	_ = conn.SetReadDeadline(time.Now().Add(pongWait))
	conn.SetPongHandler(func(string) error {
		if touch != nil {
			touch(ctx)
		}
		return conn.SetReadDeadline(time.Now().Add(pongWait))
	})
}

func runWebSocketKeepalive(ctx context.Context, conn *websocket.Conn, writeMu *sync.Mutex, interval time.Duration, writeDeadline time.Duration, fail func(error)) {
	ticker := time.NewTicker(interval)
	defer ticker.Stop()
	for {
		select {
		case <-ctx.Done():
			return
		case <-ticker.C:
			if err := writeWSControl(conn, writeMu, websocket.PingMessage, nil, writeDeadline); err != nil {
				if ctx.Err() != nil {
					return
				}
				if fail != nil {
					fail(fmt.Errorf("ping write: %w", err))
				}
				return
			}
		}
	}
}

func writeWSControl(conn *websocket.Conn, mu *sync.Mutex, messageType int, payload []byte, writeDeadline time.Duration) error {
	mu.Lock()
	defer mu.Unlock()
	return conn.WriteControl(messageType, payload, time.Now().Add(writeDeadline))
}

func writeWSMessage(conn *websocket.Conn, mu *sync.Mutex, messageType int, payload []byte, writeDeadline time.Duration) error {
	mu.Lock()
	defer mu.Unlock()
	if err := conn.SetWriteDeadline(time.Now().Add(writeDeadline)); err != nil {
		return err
	}
	return conn.WriteMessage(messageType, payload)
}
