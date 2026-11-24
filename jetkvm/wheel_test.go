// Copyright 2026 The Panio Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

package main

import (
	"bytes"
	"context"
	"strings"
	"testing"
	"time"
)

// waitRecorded waits for the fake device to have seen want. HID-RPC frames
// have no reply, so they can land after the command returns.
func waitRecorded(t *testing.T, d *fakeDevice, want []string) {
	t.Helper()
	deadline := time.Now().Add(5 * time.Second)
	var got []string
	for {
		got = d.recorded()
		if len(got) >= len(want) || time.Now().After(deadline) {
			break
		}
		time.Sleep(10 * time.Millisecond)
	}
	// Give a stray extra frame a moment to show up before comparing.
	time.Sleep(50 * time.Millisecond)
	got = d.recorded()
	if strings.Join(got, "\n") != strings.Join(want, "\n") {
		t.Fatalf("device saw:\n%s\nwant:\n%s", strings.Join(got, "\n"), strings.Join(want, "\n"))
	}
}

func hidChannels(d *fakeDevice) int {
	d.mu.Lock()
	defer d.mu.Unlock()
	return len(d.hidLabels)
}

func TestWheelFrameIsSignedYThenX(t *testing.T) {
	for _, c := range []struct {
		y, x int8
		want []byte
	}{
		{1, 0, []byte{0x04, 0x01, 0x00}},
		{-1, 0, []byte{0x04, 0xff, 0x00}},
		{-127, 0, []byte{0x04, 0x81, 0x00}},
		{127, -1, []byte{0x04, 0x7f, 0xff}},
	} {
		if got := wheelFrame(c.y, c.x); !bytes.Equal(got, c.want) {
			t.Errorf("wheelFrame(%d, %d) = % x, want % x", c.y, c.x, got, c.want)
		}
	}
}

// Firmware up to 0.5.10: the wheel stays on JSON-RPC and no HID-RPC channel
// is opened.
func TestScrollOnOldFirmwareUsesJSONRPC(t *testing.T) {
	d := newFakeDevice(t, "pw")
	l := dialFake(t, d)
	s := NewSession(linkCaller{l})
	s.t = timing{}
	mustExec(t, s, "scroll -200")
	mustExec(t, s, "scroll 5")
	mustExec(t, s, "scroll 0")
	waitRecorded(t, d, []string{
		`wheelReport {"wheelX":0,"wheelY":-127}`,
		`wheelReport {"wheelX":0,"wheelY":-73}`,
		`wheelReport {"wheelX":0,"wheelY":5}`,
	})
	if n := hidChannels(d); n != 0 {
		t.Fatalf("old firmware must not get a hidrpc channel; %d opened", n)
	}
}

// Firmware after jetkvm/kvm#1462: the first wheel step learns that JSON-RPC
// wheelReport is gone, then every step goes as a binary frame on one
// handshaken "hidrpc" channel. Other input stays on JSON-RPC.
func TestScrollOnNewFirmwareFallsBackToHIDRPC(t *testing.T) {
	d := newFakeDevice(t, "pw")
	d.noJSONWheel = true
	l := dialFake(t, d)
	s := NewSession(linkCaller{l})
	s.t = timing{}
	mustExec(t, s, "scroll -200")
	mustExec(t, s, "scroll 3")
	mustExec(t, s, "scroll 0")
	mustExec(t, s, "click")
	mustExec(t, s, "scroll 127")
	waitRecorded(t, d, []string{
		`wheelReport {"wheelX":0,"wheelY":-127}`, // refused: method not found
		`hidrpc handshake`,
		`hidrpc wheel {"wheelX":0,"wheelY":-127}`,
		`hidrpc wheel {"wheelX":0,"wheelY":-73}`,
		`hidrpc wheel {"wheelX":0,"wheelY":3}`,
		`relMouseReport {"buttons":1,"dx":0,"dy":0}`,
		`relMouseReport {"buttons":0,"dx":0,"dy":0}`,
		`hidrpc wheel {"wheelX":0,"wheelY":127}`,
	})
	if n := hidChannels(d); n != 1 {
		t.Fatalf("want one hidrpc channel, got %d", n)
	}
}

// A reconnect is a new session, which probes again: the device may have been
// upgraded or downgraded in between.
func TestScrollProbesAgainAfterReconnect(t *testing.T) {
	d := newFakeDevice(t, "pw")
	d.noJSONWheel = true
	o := testOwner(t, func(ctx context.Context) (rpcLink, error) { return Dial(ctx, d.dialConfig()) })
	send := func(line string) {
		t.Helper()
		if _, err := o.Send(context.Background(), line); err != nil {
			t.Fatalf("%q: %v", line, err)
		}
	}
	send("scroll 1")
	waitRecorded(t, d, []string{
		`wheelReport {"wheelX":0,"wheelY":1}`,
		`hidrpc handshake`,
		`hidrpc wheel {"wheelX":0,"wheelY":1}`,
	})
	first := o.liveLink()
	first.Close()

	// The device is downgraded while Panio is away.
	d.mu.Lock()
	d.noJSONWheel = false
	d.mu.Unlock()
	send("scroll -1")
	if o.liveLink() == first {
		t.Fatal("expected a new session")
	}
	waitRecorded(t, d, []string{
		`wheelReport {"wheelX":0,"wheelY":1}`,
		`hidrpc handshake`,
		`hidrpc wheel {"wheelX":0,"wheelY":1}`,
		`wheelReport {"wheelX":0,"wheelY":-1}`,
	})
}

func TestWheelReportRefusesOutOfRangeSteps(t *testing.T) {
	d := newFakeDevice(t, "pw")
	d.noJSONWheel = true
	l := dialFake(t, d)
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	if _, err := l.Call(ctx, "wheelReport", map[string]any{"wheelY": 200, "wheelX": 0}); err == nil {
		t.Fatal("a step outside int8 must be refused, not wrapped")
	}
}
