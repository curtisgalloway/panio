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
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"log/slog"
	"sync"
	"time"

	"github.com/pion/webrtc/v4"
)

// The wheel has two transports, and which one works depends on the firmware:
//
//   - Firmware up to 0.5.10 takes the JSON-RPC method `wheelReport` on the
//     "rpc" channel, like every other report.
//   - Later firmware (jetkvm/kvm#1462, first in 0.5.11-dev) removed that
//     method. It takes a binary HID-RPC frame [0x04, deltaY, deltaX] (signed
//     bytes) on the reliable, ordered "hidrpc" data channel, which the client
//     opens and handshakes ([0x01, version], answered the same way).
//
// Older firmware already has a "hidrpc" channel but silently drops wheel
// frames on it, so the binary path cannot be the default. Each session tries
// JSON-RPC first. When the device answers "method not found", the session
// opens "hidrpc", handshakes, and sends every later wheel step there. A new
// session (a reconnect) probes again, since the firmware may have changed.
//
// The unreliable HID-RPC channels are never used for the wheel: a wheel report
// is a relative step, and a dropped one would be lost for good.

const (
	hidRPCLabel       = "hidrpc"
	hidRPCVersion     = 0x01
	hidTypeHandshake  = 0x01
	hidTypeWheel      = 0x04
	rpcMethodNotFound = -32601
	// handshakeRetry is how often the handshake is resent until answered,
	// as the JetKVM web UI does: the device may not have registered its
	// message handler when the channel first opens.
	handshakeRetry = 500 * time.Millisecond
)

// wheelParams is the JSON-RPC wheelReport payload. Values outside int8 are
// refused by the decoder rather than wrapped.
type wheelParams struct {
	WheelY int8 `json:"wheelY"`
	WheelX int8 `json:"wheelX"`
}

// wheelFrame is the binary HID-RPC wheel report.
func wheelFrame(y, x int8) []byte {
	return []byte{hidTypeWheel, byte(y), byte(x)}
}

// hidChannel is the client side of the reliable "hidrpc" data channel.
type hidChannel struct {
	dc        *webrtc.DataChannel
	open      chan struct{}
	ready     chan struct{} // closed when the device answers the handshake
	openOnce  sync.Once
	readyOnce sync.Once
}

func (l *Link) wheelReport(ctx context.Context, params any) (json.RawMessage, error) {
	l.hidMu.Lock()
	defer l.hidMu.Unlock()
	if !l.wheelBinary {
		r, err := l.call(ctx, "wheelReport", params)
		var rpcErr *RPCError
		if !errors.As(err, &rpcErr) || rpcErr.Code != rpcMethodNotFound {
			return r, err
		}
		slog.Info("JetKVM firmware has no wheelReport JSON-RPC; scrolling over the binary HID-RPC channel")
		l.wheelBinary = true
	}

	b, err := json.Marshal(params)
	if err != nil {
		return nil, fmt.Errorf("wheelReport: %w", err)
	}
	var p wheelParams
	if err := json.Unmarshal(b, &p); err != nil {
		return nil, fmt.Errorf("wheelReport: %w", err)
	}
	h, err := l.hidReady(ctx)
	if err != nil {
		return nil, fmt.Errorf("wheelReport: %w", err)
	}
	if err := h.dc.Send(wheelFrame(p.WheelY, p.WheelX)); err != nil {
		return nil, fmt.Errorf("wheelReport: %w", err)
	}
	return json.RawMessage("null"), nil
}

// hidReady returns the handshaken "hidrpc" channel, opening it first when
// needed. Callers hold hidMu.
func (l *Link) hidReady(ctx context.Context) (*hidChannel, error) {
	if l.hid == nil {
		if l.pc == nil {
			return nil, errors.New("no WebRTC session for HID-RPC")
		}
		dc, err := l.pc.CreateDataChannel(hidRPCLabel, nil)
		if err != nil {
			return nil, fmt.Errorf("opening %s channel: %w", hidRPCLabel, err)
		}
		h := &hidChannel{dc: dc, open: make(chan struct{}), ready: make(chan struct{})}
		dc.OnOpen(func() { h.openOnce.Do(func() { close(h.open) }) })
		dc.OnMessage(func(m webrtc.DataChannelMessage) {
			if len(m.Data) >= 2 && m.Data[0] == hidTypeHandshake {
				h.readyOnce.Do(func() {
					slog.Info("JetKVM HID-RPC handshake done", "version", m.Data[1])
					close(h.ready)
				})
			}
		})
		l.hid = h
	}
	h := l.hid
	select {
	case <-h.ready:
		return h, nil
	default:
	}
	fail := func(err error) (*hidChannel, error) {
		// Start over next time rather than wait on a channel that never came up.
		_ = h.dc.Close()
		l.hid = nil
		return nil, err
	}
	select {
	case <-h.open:
	case <-ctx.Done():
		return fail(fmt.Errorf("%s channel did not open: %w", hidRPCLabel, ctx.Err()))
	case <-l.done:
		return nil, l.closedErr()
	}
	t := time.NewTicker(handshakeRetry)
	defer t.Stop()
	for {
		if err := h.dc.Send([]byte{hidTypeHandshake, hidRPCVersion}); err != nil {
			return fail(fmt.Errorf("%s handshake: %w", hidRPCLabel, err))
		}
		select {
		case <-h.ready:
			return h, nil
		case <-t.C:
		case <-ctx.Done():
			return fail(fmt.Errorf("%s handshake not answered: %w", hidRPCLabel, ctx.Err()))
		case <-l.done:
			return nil, l.closedErr()
		}
	}
}
