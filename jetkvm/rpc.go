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
	"encoding/base64"
	"encoding/json"
	"errors"
	"fmt"
	"log/slog"
	"net"
	"net/http"
	"net/http/cookiejar"
	"net/url"
	"strconv"
	"strings"
	"sync"
	"sync/atomic"
	"time"

	"github.com/coder/websocket"
	"github.com/coder/websocket/wsjson"
	"github.com/pion/rtcp"
	"github.com/pion/webrtc/v4"
)

// JetKVM allows ONE session at a time: a new offer kicks the current one,
// which is told so with an `otherSessionConnected` event and closed.

// ErrAuth is a rejected or missing credential. main maps it to exit 3.
var ErrAuth = errors.New("JetKVM rejected the password")

// ErrKicked means another client (usually a browser) took over the session.
var ErrKicked = errors.New("session taken over by another client")

// RPCError is a JSON-RPC error object returned by the device.
type RPCError struct {
	Code    int    `json:"code"`
	Message string `json:"message"`
}

func (e *RPCError) Error() string {
	return fmt.Sprintf("jetkvm rpc error %d: %s", e.Code, e.Message)
}

type rpcMessage struct {
	ID     int             `json:"id"`
	Result json.RawMessage `json:"result"`
	Error  *RPCError       `json:"error"`
	Method string          `json:"method"`
	Params json.RawMessage `json:"params"`
}

// DialConfig describes how to reach a device.
type DialConfig struct {
	Host     string // host[:port], http only
	Password string
	// SettingEngine adjusts the WebRTC stack; tests use it to allow loopback
	// ICE candidates. Nil in production.
	SettingEngine *webrtc.SettingEngine
	// Timeout bounds login plus the WebRTC handshake (default 20 s).
	Timeout time.Duration
	// Video, when set, adds an H.264 receive-only video track to the session
	// and feeds it to this pipeline.
	Video *Video
}

// defaultSettingEngine applies to Dial calls that do not set their own. Nil in
// production; tests set it so the CLI's own dial path can reach a loopback
// fake device.
var defaultSettingEngine *webrtc.SettingEngine

// splitHost validates a `host[:port]` device address and returns the
// authority to put in a URL. Anything URL-shaped is rejected.
func splitHost(s string) (string, error) {
	s = strings.TrimSpace(s)
	if s == "" {
		return "", errors.New("device address is empty")
	}
	if strings.ContainsAny(s, "/@?#\\ ") || strings.Contains(s, "://") {
		return "", fmt.Errorf("device address must be host[:port], got %q", s)
	}
	if strings.HasPrefix(s, "[") {
		if _, _, err := net.SplitHostPort(s); err != nil {
			if strings.HasSuffix(s, "]") {
				return s, nil
			}
			return "", fmt.Errorf("bad device address %q", s)
		}
		return s, nil
	}
	if strings.Count(s, ":") > 1 {
		return "", fmt.Errorf("bracket an IPv6 address: [%s]", s)
	}
	if h, p, err := net.SplitHostPort(s); err == nil {
		if h == "" || p == "" {
			return "", fmt.Errorf("bad device address %q", s)
		}
		if n, err := strconv.Atoi(p); err != nil || n < 1 || n > 65535 {
			return "", fmt.Errorf("bad port in device address %q", s)
		}
	} else if strings.Contains(s, ":") {
		return "", fmt.Errorf("bad device address %q", s)
	}
	return s, nil
}

// Link is one live WebRTC session with a JetKVM, carrying JSON-RPC over the
// "rpc" data channel.
type Link struct {
	pc     *webrtc.PeerConnection
	dc     *webrtc.DataChannel
	ws     *websocket.Conn
	ctx    context.Context
	cancel context.CancelFunc

	mu      sync.Mutex
	nextID  int
	waiters map[int]chan rpcMessage

	done      chan struct{}
	closeOnce sync.Once
	kicked    atomic.Bool

	// OnEvent receives server-pushed notifications (usbState, ...). It is
	// called from the data-channel goroutine and must not block.
	OnEvent func(method string, params json.RawMessage)

	video *Video
}

// h264Codecs are the only video codecs offered: H.265 is never negotiated.
func h264Codecs() []webrtc.RTPCodecParameters {
	var out []webrtc.RTPCodecParameters
	for i, profile := range []string{"42e01f", "42001f", "4d001f", "640c1f"} {
		out = append(out, webrtc.RTPCodecParameters{
			RTPCodecCapability: webrtc.RTPCodecCapability{
				MimeType:  webrtc.MimeTypeH264,
				ClockRate: 90000,
				SDPFmtpLine: "level-asymmetry-allowed=1;packetization-mode=1;" +
					"profile-level-id=" + profile,
				RTCPFeedback: []webrtc.RTCPFeedback{{Type: "nack"}, {Type: "nack", Parameter: "pli"}},
			},
			PayloadType: webrtc.PayloadType(102 + 2*i),
		})
	}
	return out
}

// RequestKeyframe sends an RTCP Picture Loss Indication for the video track.
func (l *Link) requestKeyframe(ssrc uint32) {
	if l.pc == nil {
		return
	}
	_ = l.pc.WriteRTCP([]rtcp.Packet{&rtcp.PictureLossIndication{MediaSSRC: ssrc}})
}

// Done is closed when the session ends for any reason.
func (l *Link) Done() <-chan struct{} { return l.done }

// Kicked reports whether another client took the session.
func (l *Link) Kicked() bool { return l.kicked.Load() }

// Close ends the session. Safe to call repeatedly.
func (l *Link) Close() {
	l.closeOnce.Do(func() {
		close(l.done)
		l.cancel()
		if l.ws != nil {
			_ = l.ws.CloseNow()
		}
		if l.pc != nil {
			_ = l.pc.Close()
		}
		l.mu.Lock()
		for id, ch := range l.waiters {
			close(ch)
			delete(l.waiters, id)
		}
		l.mu.Unlock()
	})
}

// Call sends one JSON-RPC request and waits for its response.
func (l *Link) Call(ctx context.Context, method string, params any) (json.RawMessage, error) {
	select {
	case <-l.done:
		return nil, l.closedErr()
	default:
	}
	l.mu.Lock()
	l.nextID++
	id := l.nextID
	ch := make(chan rpcMessage, 1)
	l.waiters[id] = ch
	l.mu.Unlock()

	req := map[string]any{"jsonrpc": "2.0", "method": method, "id": id}
	if params != nil {
		req["params"] = params
	}
	b, err := json.Marshal(req)
	if err != nil {
		return nil, err
	}
	forget := func() {
		l.mu.Lock()
		delete(l.waiters, id)
		l.mu.Unlock()
	}
	if err := l.dc.SendText(string(b)); err != nil {
		forget()
		return nil, fmt.Errorf("%s: %w", method, err)
	}
	select {
	case r, ok := <-ch:
		if !ok {
			return nil, l.closedErr()
		}
		if r.Error != nil {
			return nil, r.Error
		}
		return r.Result, nil
	case <-ctx.Done():
		forget()
		return nil, fmt.Errorf("%s: %w", method, ctx.Err())
	case <-l.done:
		forget()
		return nil, l.closedErr()
	}
}

func (l *Link) closedErr() error {
	if l.Kicked() {
		return ErrKicked
	}
	return errors.New("jetkvm session closed")
}

func (l *Link) onMessage(data []byte) {
	var m rpcMessage
	if json.Unmarshal(data, &m) != nil {
		return
	}
	if m.Method != "" {
		if m.Method == "otherSessionConnected" {
			l.kicked.Store(true)
			slog.Warn("another client took the JetKVM session; closing ours")
			l.Close()
			return
		}
		if m.Method == "videoInputState" && l.video != nil {
			l.video.InputState(m.Params)
		}
		if l.OnEvent != nil {
			l.OnEvent(m.Method, m.Params)
		}
		return
	}
	l.mu.Lock()
	ch := l.waiters[m.ID]
	delete(l.waiters, m.ID)
	l.mu.Unlock()
	if ch != nil {
		ch <- m
	}
}

type signalMsg struct {
	Type  string          `json:"type"`
	Data  json.RawMessage `json:"data"`
	Error json.RawMessage `json:"error"`
}

func login(ctx context.Context, authority, password string) (*http.Client, error) {
	jar, err := cookiejar.New(nil)
	if err != nil {
		return nil, err
	}
	hc := &http.Client{Jar: jar, Timeout: 10 * time.Second}
	body, _ := json.Marshal(map[string]string{"password": password})
	req, err := http.NewRequestWithContext(ctx, http.MethodPost,
		"http://"+authority+"/auth/login-local", strings.NewReader(string(body)))
	if err != nil {
		return nil, err
	}
	req.Header.Set("Content-Type", "application/json")
	resp, err := hc.Do(req)
	if err != nil {
		return nil, fmt.Errorf("login: %w", err)
	}
	defer resp.Body.Close()
	switch {
	case resp.StatusCode == http.StatusUnauthorized || resp.StatusCode == http.StatusForbidden:
		return nil, ErrAuth
	case resp.StatusCode != http.StatusOK:
		return nil, fmt.Errorf("login: HTTP %d", resp.StatusCode)
	}
	return hc, nil
}

// Dial logs in, opens the signaling WebSocket, negotiates a data-channel-only
// WebRTC session and returns once the "rpc" channel is open.
func Dial(parent context.Context, cfg DialConfig) (*Link, error) {
	authority, err := splitHost(cfg.Host)
	if err != nil {
		return nil, err
	}
	timeout := cfg.Timeout
	if timeout == 0 {
		timeout = 20 * time.Second
	}
	ctx, cancelHandshake := context.WithTimeout(parent, timeout)
	defer cancelHandshake()

	hc, err := login(ctx, authority, cfg.Password)
	if err != nil {
		return nil, err
	}
	u, _ := url.Parse("http://" + authority + "/")
	hdr := http.Header{}
	for _, ck := range hc.Jar.Cookies(u) {
		hdr.Add("Cookie", ck.Name+"="+ck.Value)
	}
	ws, _, err := websocket.Dial(ctx, "ws://"+authority+"/webrtc/signaling/client",
		&websocket.DialOptions{HTTPHeader: hdr})
	if err != nil {
		return nil, fmt.Errorf("signaling dial: %w", err)
	}

	se := cfg.SettingEngine
	if se == nil {
		se = defaultSettingEngine
	}
	opts := []func(*webrtc.API){}
	if se != nil {
		opts = append(opts, webrtc.WithSettingEngine(*se))
	}
	if cfg.Video != nil {
		me := &webrtc.MediaEngine{}
		for _, c := range h264Codecs() {
			if err := me.RegisterCodec(c, webrtc.RTPCodecTypeVideo); err != nil {
				_ = ws.CloseNow()
				return nil, err
			}
		}
		opts = append(opts, webrtc.WithMediaEngine(me))
	}
	api := webrtc.NewAPI(opts...)
	pc, err := api.NewPeerConnection(webrtc.Configuration{})
	if err != nil {
		_ = ws.CloseNow()
		return nil, err
	}
	lctx, lcancel := context.WithCancel(context.Background())
	l := &Link{
		pc: pc, ws: ws, ctx: lctx, cancel: lcancel,
		waiters: map[int]chan rpcMessage{}, done: make(chan struct{}),
		video: cfg.Video,
	}
	fail := func(err error) (*Link, error) {
		l.Close()
		return nil, err
	}

	if cfg.Video != nil {
		if _, err := pc.AddTransceiverFromKind(webrtc.RTPCodecTypeVideo,
			webrtc.RTPTransceiverInit{Direction: webrtc.RTPTransceiverDirectionRecvonly}); err != nil {
			return fail(err)
		}
		pc.OnTrack(func(tr *webrtc.TrackRemote, _ *webrtc.RTPReceiver) {
			if tr.Kind() != webrtc.RTPCodecTypeVideo {
				return
			}
			slog.Info("JetKVM video track", "codec", tr.Codec().MimeType)
			ssrc := uint32(tr.SSRC())
			cfg.Video.Attach(func() { l.requestKeyframe(ssrc) })
			for {
				pkt, _, err := tr.ReadRTP()
				if err != nil {
					return
				}
				cfg.Video.Packet(pkt)
			}
		})
	}
	dc, err := pc.CreateDataChannel("rpc", nil)
	if err != nil {
		return fail(err)
	}
	l.dc = dc
	open := make(chan struct{})
	var openOnce sync.Once
	dc.OnOpen(func() { openOnce.Do(func() { close(open) }) })
	dc.OnMessage(func(m webrtc.DataChannelMessage) { l.onMessage(m.Data) })
	dc.OnClose(l.Close)
	pc.OnConnectionStateChange(func(s webrtc.PeerConnectionState) {
		if s == webrtc.PeerConnectionStateFailed || s == webrtc.PeerConnectionStateClosed {
			l.Close()
		}
	})

	offer, err := pc.CreateOffer(nil)
	if err != nil {
		return fail(err)
	}
	gathered := webrtc.GatheringCompletePromise(pc)
	if err := pc.SetLocalDescription(offer); err != nil {
		return fail(err)
	}
	select {
	case <-gathered:
	case <-ctx.Done():
		return fail(fmt.Errorf("ICE gathering: %w", ctx.Err()))
	}
	ld, err := json.Marshal(pc.LocalDescription())
	if err != nil {
		return fail(err)
	}
	if err := wsjson.Write(ctx, ws, map[string]any{
		"type": "offer",
		"data": map[string]string{"sd": base64.StdEncoding.EncodeToString(ld)},
	}); err != nil {
		return fail(fmt.Errorf("sending offer: %w", err))
	}

	var early []webrtc.ICECandidateInit
	gotAnswer := false
	for !gotAnswer {
		var msg signalMsg
		if err := wsjson.Read(ctx, ws, &msg); err != nil {
			return fail(fmt.Errorf("signaling read: %w", err))
		}
		switch msg.Type {
		case "answer":
			var sd string
			if err := json.Unmarshal(msg.Data, &sd); err != nil {
				return fail(fmt.Errorf("answer: %w", err))
			}
			raw, err := base64.StdEncoding.DecodeString(sd)
			if err != nil {
				return fail(fmt.Errorf("answer: %w", err))
			}
			var ans webrtc.SessionDescription
			if err := json.Unmarshal(raw, &ans); err != nil {
				return fail(fmt.Errorf("answer: %w", err))
			}
			if err := pc.SetRemoteDescription(ans); err != nil {
				return fail(err)
			}
			gotAnswer = true
		case "new-ice-candidate":
			var c webrtc.ICECandidateInit
			if json.Unmarshal(msg.Data, &c) == nil {
				early = append(early, c)
			}
		case "device-metadata":
		default:
			if len(msg.Error) > 0 {
				return fail(fmt.Errorf("signaling error: %s", msg.Error))
			}
		}
	}
	for _, c := range early {
		_ = pc.AddICECandidate(c)
	}
	go func() {
		for {
			var m signalMsg
			if wsjson.Read(lctx, ws, &m) != nil {
				return
			}
			if m.Type == "new-ice-candidate" {
				var c webrtc.ICECandidateInit
				if json.Unmarshal(m.Data, &c) == nil {
					_ = pc.AddICECandidate(c)
				}
			}
		}
	}()
	select {
	case <-open:
		return l, nil
	case <-ctx.Done():
		return fail(errors.New("rpc data channel did not open in time"))
	}
}
