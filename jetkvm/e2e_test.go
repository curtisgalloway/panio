// Copyright 2026 Curtis Galloway
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
	"encoding/base64"
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"strconv"
	"strings"
	"sync"
	"sync/atomic"
	"testing"
	"time"

	"github.com/coder/websocket"
	"github.com/coder/websocket/wsjson"
	"github.com/pion/ice/v4"
	"github.com/pion/rtcp"
	"github.com/pion/webrtc/v4"
)

// fakeDevice stands in for a JetKVM: the same HTTP login, signaling WebSocket
// and "rpc" data channel the real firmware serves, built on a pion answerer.
type fakeDevice struct {
	t        *testing.T
	srv      *httptest.Server
	password string

	mu       sync.Mutex
	calls    []string // "method {json params}", in arrival order
	sessions []*fakeSession
	logins   int

	offers  []string // SDP of every offer received
	videoAU [][]byte // when set, answer with an H.264 track playing these access units
	plis    atomic.Int32
}

type fakeSession struct {
	pc *webrtc.PeerConnection
	dc *webrtc.DataChannel
}

func loopbackEngine() *webrtc.SettingEngine {
	se := webrtc.SettingEngine{}
	se.SetIncludeLoopbackCandidate(true)
	se.SetNetworkTypes([]webrtc.NetworkType{webrtc.NetworkTypeUDP4})
	se.SetICEMulticastDNSMode(ice.MulticastDNSModeDisabled)
	return &se
}

func newFakeDevice(t *testing.T, password string) *fakeDevice {
	d := &fakeDevice{t: t, password: password}
	mux := http.NewServeMux()
	mux.HandleFunc("POST /auth/login-local", d.login)
	mux.HandleFunc("GET /webrtc/signaling/client", d.signaling)
	d.srv = httptest.NewServer(mux)
	t.Cleanup(func() {
		d.mu.Lock()
		for _, s := range d.sessions {
			_ = s.pc.Close()
		}
		d.mu.Unlock()
		d.srv.Close()
	})
	return d
}

func (d *fakeDevice) host() string { return strings.TrimPrefix(d.srv.URL, "http://") }

func (d *fakeDevice) login(w http.ResponseWriter, r *http.Request) {
	var body struct{ Password string }
	_ = json.NewDecoder(r.Body).Decode(&body)
	if body.Password != d.password {
		http.Error(w, `{"error":"Invalid password"}`, http.StatusUnauthorized)
		return
	}
	d.mu.Lock()
	d.logins++
	d.mu.Unlock()
	http.SetCookie(w, &http.Cookie{Name: "authToken", Value: "tok", Path: "/"})
	_, _ = w.Write([]byte(`{"message":"Logged in"}`))
}

func (d *fakeDevice) signaling(w http.ResponseWriter, r *http.Request) {
	if c, err := r.Cookie("authToken"); err != nil || c.Value != "tok" {
		http.Error(w, "unauthorized", http.StatusUnauthorized)
		return
	}
	ws, err := websocket.Accept(w, r, &websocket.AcceptOptions{InsecureSkipVerify: true})
	if err != nil {
		return
	}
	ctx := context.Background()
	_ = wsjson.Write(ctx, ws, map[string]any{"type": "device-metadata", "data": map[string]any{"deviceVersion": "0.5.9"}})
	var msg struct {
		Type string
		Data struct {
			SD string `json:"sd"`
		}
	}
	if wsjson.Read(ctx, ws, &msg) != nil || msg.Type != "offer" {
		return
	}
	raw, _ := base64.StdEncoding.DecodeString(msg.Data.SD)
	var offer webrtc.SessionDescription
	if json.Unmarshal(raw, &offer) != nil {
		return
	}

	// One session at a time: tell the current one it was replaced.
	d.mu.Lock()
	for _, old := range d.sessions {
		if old.dc != nil && old.dc.ReadyState() == webrtc.DataChannelStateOpen {
			_ = old.dc.SendText(`{"jsonrpc":"2.0","method":"otherSessionConnected"}`)
		}
	}
	d.mu.Unlock()

	d.mu.Lock()
	d.offers = append(d.offers, offer.SDP)
	videoAU := d.videoAU
	d.mu.Unlock()
	opts := []func(*webrtc.API){webrtc.WithSettingEngine(*loopbackEngine())}
	if videoAU != nil {
		me := &webrtc.MediaEngine{}
		for _, c := range h264Codecs() {
			_ = me.RegisterCodec(c, webrtc.RTPCodecTypeVideo)
		}
		opts = append(opts, webrtc.WithMediaEngine(me))
	}
	api := webrtc.NewAPI(opts...)
	pc, err := api.NewPeerConnection(webrtc.Configuration{})
	if err != nil {
		return
	}
	sess := &fakeSession{pc: pc}
	d.mu.Lock()
	d.sessions = append(d.sessions, sess)
	d.mu.Unlock()
	pc.OnDataChannel(func(dc *webrtc.DataChannel) {
		if dc.Label() != "rpc" {
			return
		}
		d.mu.Lock()
		sess.dc = dc
		d.mu.Unlock()
		dc.OnMessage(func(m webrtc.DataChannelMessage) { d.handle(dc, m.Data) })
	})
	if pc.SetRemoteDescription(offer) != nil {
		return
	}
	if videoAU != nil {
		track, err := webrtc.NewTrackLocalStaticRTP(webrtc.RTPCodecCapability{
			MimeType: webrtc.MimeTypeH264, ClockRate: 90000,
			SDPFmtpLine: "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f",
		}, "video", "jetkvm")
		if err != nil {
			return
		}
		sender, err := pc.AddTrack(track)
		if err != nil {
			return
		}
		go func() { // count PLIs
			for {
				pkts, _, err := sender.ReadRTCP()
				if err != nil {
					return
				}
				for _, p := range pkts {
					if _, ok := p.(*rtcp.PictureLossIndication); ok {
						d.plis.Add(1)
					}
				}
			}
		}()
		pc.OnConnectionStateChange(func(s webrtc.PeerConnectionState) {
			if s == webrtc.PeerConnectionStateConnected {
				go d.playVideo(pc, track, videoAU)
			}
		})
	}
	answer, err := pc.CreateAnswer(nil)
	if err != nil {
		return
	}
	gathered := webrtc.GatheringCompletePromise(pc)
	if pc.SetLocalDescription(answer) != nil {
		return
	}
	<-gathered
	ld, _ := json.Marshal(pc.LocalDescription())
	_ = wsjson.Write(ctx, ws, map[string]any{"type": "answer", "data": base64.StdEncoding.EncodeToString(ld)})
	// Hold the signaling socket open until the client goes away.
	for {
		if _, _, err := ws.Read(ctx); err != nil {
			return
		}
	}
}

// playVideo loops the clip as one continuous RTP stream until the peer closes.
func (d *fakeDevice) playVideo(pc *webrtc.PeerConnection, track *webrtc.TrackLocalStaticRTP, aus [][]byte) {
	stream := newRTPStream()
	for {
		for _, frame := range stream.packetize(aus) {
			for _, p := range frame {
				if pc.ConnectionState() != webrtc.PeerConnectionStateConnected {
					return
				}
				if track.WriteRTP(p) != nil {
					return
				}
			}
			time.Sleep(40 * time.Millisecond)
		}
	}
}

func (d *fakeDevice) handle(dc *webrtc.DataChannel, data []byte) {
	var req struct {
		ID     int             `json:"id"`
		Method string          `json:"method"`
		Params json.RawMessage `json:"params"`
	}
	if json.Unmarshal(data, &req) != nil {
		return
	}
	var canon any
	_ = json.Unmarshal(req.Params, &canon)
	cb, _ := json.Marshal(canon)
	d.mu.Lock()
	d.calls = append(d.calls, req.Method+" "+string(cb))
	d.mu.Unlock()
	resp := map[string]any{"jsonrpc": "2.0", "id": req.ID}
	switch req.Method {
	case "ping":
		resp["result"] = "pong"
	case "getUSBState":
		resp["result"] = "configured"
	case "getVideoState":
		resp["result"] = map[string]any{"ready": true, "width": 1920}
	case "getKeyboardLedState":
		resp["result"] = map[string]any{"num_lock": true}
	case "keyboardReport", "absMouseReport", "relMouseReport", "wheelReport":
		resp["result"] = nil
	default:
		resp["error"] = map[string]any{"code": -32601, "message": "method not found"}
	}
	b, _ := json.Marshal(resp)
	_ = dc.SendText(string(b))
}

func (d *fakeDevice) recorded() []string {
	d.mu.Lock()
	defer d.mu.Unlock()
	return append([]string(nil), d.calls...)
}

func (d *fakeDevice) dialConfig() DialConfig {
	return DialConfig{Host: d.host(), Password: d.password, SettingEngine: loopbackEngine(), Timeout: 15 * time.Second}
}

func dialFake(t *testing.T, d *fakeDevice) *Link {
	t.Helper()
	ctx, cancel := context.WithTimeout(context.Background(), 20*time.Second)
	defer cancel()
	l, err := Dial(ctx, d.dialConfig())
	if err != nil {
		t.Fatalf("dial: %v", err)
	}
	t.Cleanup(l.Close)
	return l
}

func rpc(t *testing.T, l *Link, method string, params any) json.RawMessage {
	t.Helper()
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	r, err := l.Call(ctx, method, params)
	if err != nil {
		t.Fatalf("%s: %v", method, err)
	}
	return r
}

func TestEndToEndPingAndTyping(t *testing.T) {
	d := newFakeDevice(t, "hunter2")
	l := dialFake(t, d)
	if got := string(rpc(t, l, "ping", nil)); got != `"pong"` {
		t.Fatalf("ping: %s", got)
	}
	s := NewSession(linkCaller{l})
	s.t = timing{}
	mustExec(t, s, "type Hi")
	mustExec(t, s, "moveabs 100 200")
	mustExec(t, s, "click")
	want := []string{
		`ping null`,
		`keyboardReport {"keys":[11],"modifier":2}`,
		`keyboardReport {"keys":[],"modifier":0}`,
		`keyboardReport {"keys":[12],"modifier":0}`,
		`keyboardReport {"keys":[],"modifier":0}`,
		`absMouseReport {"buttons":0,"x":100,"y":200}`,
		`relMouseReport {"buttons":1,"dx":0,"dy":0}`,
		`relMouseReport {"buttons":0,"dx":0,"dy":0}`,
	}
	got := d.recorded()
	if strings.Join(got, "\n") != strings.Join(want, "\n") {
		t.Fatalf("device saw:\n%s\nwant:\n%s", strings.Join(got, "\n"), strings.Join(want, "\n"))
	}
}

func TestEndToEndRPCErrorIsTyped(t *testing.T) {
	d := newFakeDevice(t, "pw")
	l := dialFake(t, d)
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	_, err := l.Call(ctx, "noSuchMethod", nil)
	if _, ok := err.(*RPCError); !ok {
		t.Fatalf("want *RPCError, got %T %v", err, err)
	}
}

func TestEndToEndWrongPasswordIsAuthError(t *testing.T) {
	d := newFakeDevice(t, "right")
	cfg := d.dialConfig()
	cfg.Password = "wrong"
	_, err := Dial(context.Background(), cfg)
	if err != ErrAuth {
		t.Fatalf("got %v", err)
	}
	// And the CLI maps that to exit 3.
	var stderr strings.Builder
	if code := report(err, &stderr); code != exitNotConfigured {
		t.Fatalf("exit %d", code)
	}
}

// The firmware's one-session rule: a second client kicks the first, which is
// told so and must not look like a plain network drop.
func TestEndToEndSecondSessionKicksTheFirst(t *testing.T) {
	d := newFakeDevice(t, "pw")
	first := dialFake(t, d)
	second := dialFake(t, d)
	select {
	case <-first.Done():
	case <-time.After(5 * time.Second):
		t.Fatal("the first session was not closed")
	}
	if !first.Kicked() {
		t.Fatal("the first session must know it was kicked")
	}
	if second.Kicked() {
		t.Fatal("the second session is the live one")
	}
	if got := string(rpc(t, second, "ping", nil)); got != `"pong"` {
		t.Fatalf("second ping: %s", got)
	}
}

func TestEndToEndDaemonViaCLI(t *testing.T) {
	if testing.Short() {
		t.Skip("spawns the daemon")
	}
	d := newFakeDevice(t, "pw")
	t.Setenv("PANIO_RUNTIME_DIR", t.TempDir())
	t.Setenv(passwordEnv, "pw")

	// Run the daemon's pieces directly (a real SIGTERM handler would be
	// process-global) but through the same serveDaemon the CLI uses.
	owner := NewOwner(d.host(), func(ctx context.Context) (rpcLink, error) {
		return Dial(ctx, d.dialConfig())
	})
	done := make(chan error, 1)
	go func() { done <- serveDaemon(d.host(), "", 0, owner) }()

	var disc *Discovery
	deadline := time.Now().Add(10 * time.Second)
	for disc = discover(""); disc == nil && time.Now().Before(deadline); disc = discover("") {
		time.Sleep(20 * time.Millisecond)
	}
	if disc == nil || disc.Token == "" || disc.Device != d.host() {
		t.Fatalf("discovery: %+v", disc)
	}

	// One-shots for the same device route through the daemon: the fake device
	// sees exactly ONE login however many commands run.
	var out, errb strings.Builder
	for _, args := range [][]string{
		{"-d", d.host(), "ping"},
		{"-d", d.host(), "type", "a"},
		{"-d", d.host(), "key", "ENTER"},
	} {
		out.Reset()
		if code := run(args, strings.NewReader(""), &out, &errb); code != 0 || strings.TrimSpace(out.String()) != "OK" {
			t.Fatalf("%v: exit %d out %q err %q", args, code, out.String(), errb.String())
		}
	}
	d.mu.Lock()
	logins := d.logins
	d.mu.Unlock()
	if logins != 1 {
		t.Fatalf("the daemon must hold ONE session; device saw %d logins", logins)
	}

	// An error from the device comes back as a failing exit.
	if code := run([]string{"-d", d.host(), "key", "NOPE"}, strings.NewReader(""), &out, &errb); code != exitFailure {
		t.Fatalf("exit %d", code)
	}

	// The WebSocket: a command line in, an evt frame out.
	wctx, wcancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer wcancel()
	c, _, err := websocket.Dial(wctx, "ws://127.0.0.1:"+strconv.Itoa(disc.Port)+"/hid?token="+disc.Token, nil)
	if err != nil {
		t.Fatal(err)
	}
	defer c.CloseNow()
	if err := c.Write(wctx, websocket.MessageText, []byte("ping")); err != nil {
		t.Fatal(err)
	}
	_, frame, err := c.Read(wctx)
	if err != nil || string(frame) != "evt ok ping :: OK" {
		t.Fatalf("frame %q err %v", frame, err)
	}
	if err := c.Write(wctx, websocket.MessageText, []byte("key NOPE")); err != nil {
		t.Fatal(err)
	}
	_, frame, _ = c.Read(wctx)
	if !strings.HasPrefix(string(frame), "evt err key NOPE :: unknown key name") {
		t.Fatalf("frame %q", frame)
	}

	// Authenticated stop: releases held keys, removes the discovery file.
	mustSend := func(line string) {
		if _, err := owner.Send(context.Background(), line); err != nil {
			t.Fatal(err)
		}
	}
	mustSend("down A")
	out.Reset()
	if code := run([]string{"stop"}, strings.NewReader(""), &out, &errb); code != 0 || !strings.Contains(out.String(), "stopping") {
		t.Fatalf("stop: exit %d out %q err %q", code, out.String(), errb.String())
	}
	select {
	case err := <-done:
		if err != nil {
			t.Fatalf("serveDaemon: %v", err)
		}
	case <-time.After(10 * time.Second):
		t.Fatal("the daemon did not shut down")
	}
	if discover("") != nil {
		t.Fatal("the discovery file must be removed on shutdown")
	}
	calls := d.recorded()
	// The last RPCs are the release: empty keyboard report, buttons up.
	if n := len(calls); n < 2 || calls[n-2] != `keyboardReport {"keys":[],"modifier":0}` || calls[n-1] != `relMouseReport {"buttons":0,"dx":0,"dy":0}` {
		t.Fatalf("shutdown must release held keys; tail: %v", calls[max(0, len(calls)-3):])
	}
	// With the daemon gone, stop is a no-op and one-shots go direct again.
	out.Reset()
	if code := run([]string{"stop"}, strings.NewReader(""), &out, &errb); code != 0 || !strings.Contains(out.String(), "no hid daemon running") {
		t.Fatalf("second stop: %d %q", code, out.String())
	}
}
