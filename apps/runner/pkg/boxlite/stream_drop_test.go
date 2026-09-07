// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 BoxLite AI

package boxlite

import "testing"

// TestStreamBusDropsOnSlowSubscriber locks down the per-subscriber back-pressure
// drop: when a subscriber's channel is full (a stalled /attach client), the
// non-blocking fan-out drops the chunk and accounts it in Dropped() rather than
// blocking the producer or every other subscriber. This is the path behind the
// "slow consumer loses bytes" behavior; the test makes it explicit and counted.
func TestStreamBusDropsOnSlowSubscriber(t *testing.T) {
	bus := newStreamBus(1 << 20) // large backlog cap; not under test here
	const chBuf = 2
	sub, cancel := bus.Subscribe(chBuf)
	defer cancel()

	// Never read from sub.Chan(): the subscriber is stalled.
	const chunk = "0123456789" // 10 bytes
	const writes = 10
	for i := 0; i < writes; i++ {
		if _, err := bus.Write([]byte(chunk)); err != nil {
			t.Fatalf("Write %d: %v", i, err)
		}
	}

	// The first chBuf chunks buffer in the channel; every later chunk is
	// dropped and counted. The producer must never block.
	wantDropped := uint64((writes - chBuf) * len(chunk))
	if got := sub.Dropped(); got != wantDropped {
		t.Fatalf("Dropped() = %d, want %d (buffered %d chunks, dropped the rest)",
			got, wantDropped, chBuf)
	}
}
