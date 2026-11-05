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
	"encoding/binary"
	"io"
	"net"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
	"time"

	"github.com/coder/websocket"
)

// rfbFixture is a handler with a Video that never runs ffmpeg: frames are
// stored directly.
func rfbFixture(t *testing.T) (*httptest.Server, *Video) {
	t.Helper()
	o := testOwner(t, func(context.Context) (rpcLink, error) { return newFakeLink(), nil })
	v := NewVideo("ffmpeg-is-not-run-in-these-tests", "", &fakeHost{})
	t.Cleanup(v.Close)
	o.SetVideo(v)
	srv := httptest.NewServer(newHandler(o, testToken, func() {}))
	t.Cleanup(srv.Close)
	return srv, v
}

type rfbClient struct {
	t    *testing.T
	conn net.Conn
	ws   *websocket.Conn
}

func dialRFB(t *testing.T, srv *httptest.Server, protos []string) *rfbClient {
	t.Helper()
	ctx, cancel := context.WithTimeout(context.Background(), 20*time.Second)
	t.Cleanup(cancel)
	url := "ws" + strings.TrimPrefix(srv.URL, "http") + "/rfb?token=" + testToken
	ws, _, err := websocket.Dial(ctx, url, &websocket.DialOptions{Subprotocols: protos})
	if err != nil {
		t.Fatalf("dial /rfb: %v", err)
	}
	t.Cleanup(func() { ws.CloseNow() })
	return &rfbClient{t: t, conn: websocket.NetConn(ctx, ws, websocket.MessageBinary), ws: ws}
}

func (c *rfbClient) read(n int) []byte {
	c.t.Helper()
	b := make([]byte, n)
	if _, err := io.ReadFull(c.conn, b); err != nil {
		c.t.Fatalf("read %d bytes: %v", n, err)
	}
	return b
}

func (c *rfbClient) write(b ...byte) {
	c.t.Helper()
	if _, err := c.conn.Write(b); err != nil {
		c.t.Fatal(err)
	}
}

// handshake runs version/security/init and returns the ServerInit size.
func (c *rfbClient) handshake() (w, h int) {
	c.t.Helper()
	if v := string(c.read(12)); v != "RFB 003.008\n" {
		c.t.Fatalf("version %q", v)
	}
	c.write([]byte("RFB 003.008\n")...)
	if sec := c.read(2); sec[0] != 1 || sec[1] != 1 {
		c.t.Fatalf("security types %v, want [1 None]", sec)
	}
	c.write(1)
	if r := c.read(4); binary.BigEndian.Uint32(r) != 0 {
		c.t.Fatalf("SecurityResult %v", r)
	}
	c.write(1) // shared
	si := c.read(24)
	w, h = int(binary.BigEndian.Uint16(si[0:])), int(binary.BigEndian.Uint16(si[2:]))
	pf := parsePixelFormat(si[4:20])
	if pf != nativeFormat {
		c.t.Fatalf("pixel format %+v", pf)
	}
	name := c.read(int(binary.BigEndian.Uint32(si[20:])))
	if string(name) != "jetkvm" {
		c.t.Fatalf("name %q", name)
	}
	return w, h
}

func (c *rfbClient) request(incremental bool, w, h int) {
	b := []byte{3, 0, 0, 0, 0, 0, byte(w >> 8), byte(w), byte(h >> 8), byte(h)}
	if incremental {
		b[1] = 1
	}
	c.write(b...)
}

func (c *rfbClient) setEncodings(encs ...int32) {
	b := []byte{2, 0, byte(len(encs) >> 8), byte(len(encs))}
	for _, e := range encs {
		b = binary.BigEndian.AppendUint32(b, uint32(e))
	}
	c.write(b...)
}

// update reads one FramebufferUpdate and returns its rectangles.
type rect struct {
	x, y, w, h int
	enc        int32
	pix        []byte
}

func (c *rfbClient) update() []rect {
	c.t.Helper()
	hd := c.read(4)
	if hd[0] != 0 {
		c.t.Fatalf("message type %d, want FramebufferUpdate", hd[0])
	}
	var rs []rect
	for i := 0; i < int(binary.BigEndian.Uint16(hd[2:])); i++ {
		rh := c.read(12)
		r := rect{
			x: int(binary.BigEndian.Uint16(rh[0:])), y: int(binary.BigEndian.Uint16(rh[2:])),
			w: int(binary.BigEndian.Uint16(rh[4:])), h: int(binary.BigEndian.Uint16(rh[6:])),
			enc: int32(binary.BigEndian.Uint32(rh[8:])),
		}
		if r.enc == encRaw {
			r.pix = c.read(r.w * r.h * 4)
		}
		rs = append(rs, r)
	}
	return rs
}

func solid(w, h int, b, g, r byte) []byte {
	p := make([]byte, w*h*4)
	for i := 0; i < w*h; i++ {
		p[4*i], p[4*i+1], p[4*i+2] = b, g, r
	}
	return p
}

func TestRFBRequiresAuth(t *testing.T) {
	srv, _ := rfbFixture(t)
	url := "ws" + strings.TrimPrefix(srv.URL, "http") + "/rfb"
	_, resp, err := websocket.Dial(context.Background(), url, nil)
	if err == nil || resp == nil || resp.StatusCode != 401 {
		t.Fatalf("no token: err=%v resp=%v, want 401", err, resp)
	}
}

func TestRFBHandshakeAndFullRawUpdate(t *testing.T) {
	srv, v := rfbFixture(t)
	v.Store().Set(4, 2, solid(4, 2, 10, 20, 30))
	c := dialRFB(t, srv, []string{"binary"})
	if c.ws.Subprotocol() != "binary" {
		t.Fatalf("subprotocol %q, want binary", c.ws.Subprotocol())
	}
	if w, h := c.handshake(); w != 4 || h != 2 {
		t.Fatalf("ServerInit size %dx%d", w, h)
	}
	c.request(false, 4, 2)
	rs := c.update()
	if len(rs) != 1 || rs[0].enc != encRaw || rs[0].w != 4 || rs[0].h != 2 {
		t.Fatalf("rects %+v", rs)
	}
	if rs[0].pix[0] != 10 || rs[0].pix[1] != 20 || rs[0].pix[2] != 30 {
		t.Fatalf("pixel bytes % x, want 0a 14 1e (bgr)", rs[0].pix[:4])
	}
}

func TestRFBWithoutSubprotocolStillWorks(t *testing.T) {
	srv, v := rfbFixture(t)
	v.Store().Set(2, 2, solid(2, 2, 1, 2, 3))
	c := dialRFB(t, srv, nil)
	if c.ws.Subprotocol() != "" {
		t.Fatalf("subprotocol %q, want none", c.ws.Subprotocol())
	}
	c.handshake()
}

func TestRFBIncrementalRequestBlocksUntilANewFrame(t *testing.T) {
	srv, v := rfbFixture(t)
	v.Store().Set(2, 2, solid(2, 2, 1, 1, 1))
	c := dialRFB(t, srv, []string{"binary"})
	c.handshake()
	c.request(false, 2, 2)
	c.update()
	c.request(true, 2, 2)
	got := make(chan []rect, 1)
	go func() { got <- c.update() }()
	select {
	case <-got:
		t.Fatal("an incremental request was answered with no new frame")
	case <-time.After(300 * time.Millisecond):
	}
	v.Store().Set(2, 2, solid(2, 2, 9, 9, 9))
	select {
	case rs := <-got:
		if rs[0].pix[0] != 9 {
			t.Fatalf("got the old frame: % x", rs[0].pix[:4])
		}
	case <-time.After(3 * time.Second):
		t.Fatal("no update after a new frame")
	}
}

func TestRFBSetPixelFormatConvertsAndRejectsUnsupported(t *testing.T) {
	srv, v := rfbFixture(t)
	v.Store().Set(1, 1, solid(1, 1, 0x40, 0x80, 0xff)) // b, g, r
	c := dialRFB(t, srv, []string{"binary"})
	c.handshake()
	// 32bpp, big-endian, red shift 0, green 8, blue 16 (RGBA-ish), max 255.
	spf := []byte{0, 0, 0, 0, 32, 24, 1, 1, 0, 255, 0, 255, 0, 255, 0, 8, 16, 0, 0, 0}
	c.write(spf...)
	c.request(false, 1, 1)
	px := c.update()[0].pix
	// value = r | g<<8 | b<<16, big-endian: 00 b g r
	if want := []byte{0x00, 0x40, 0x80, 0xff}; string(px) != string(want) {
		t.Fatalf("converted % x, want % x", px, want)
	}

	c2 := dialRFB(t, srv, []string{"binary"})
	c2.handshake()
	bad := []byte{0, 0, 0, 0, 16, 16, 0, 1, 0, 31, 0, 63, 0, 31, 11, 5, 0, 0, 0, 0}
	c2.write(bad...)
	c2.request(false, 1, 1)
	if _, err := io.ReadFull(c2.conn, make([]byte, 1)); err == nil {
		t.Fatal("a 16bpp format should close the connection")
	}
}

func TestRFBDesktopSizeOnResize(t *testing.T) {
	srv, v := rfbFixture(t)
	v.Store().Set(2, 2, solid(2, 2, 1, 1, 1))
	c := dialRFB(t, srv, []string{"binary"})
	c.handshake()
	c.setEncodings(encRaw, encDesktopSize)
	c.request(false, 2, 2)
	c.update()
	c.request(true, 2, 2)
	v.Store().Set(4, 3, solid(4, 3, 5, 5, 5))
	rs := c.update()
	if len(rs) != 2 || rs[0].enc != encDesktopSize || rs[0].w != 4 || rs[0].h != 3 ||
		rs[1].enc != encRaw || len(rs[1].pix) != 4*3*4 {
		t.Fatalf("rects %+v", rs)
	}
}

func TestRFBResizeWithoutDesktopSizeClosesTheConnection(t *testing.T) {
	srv, v := rfbFixture(t)
	v.Store().Set(2, 2, solid(2, 2, 1, 1, 1))
	c := dialRFB(t, srv, []string{"binary"})
	c.handshake()
	c.request(false, 2, 2)
	c.update()
	c.request(true, 2, 2)
	v.Store().Set(4, 3, solid(4, 3, 5, 5, 5))
	if _, err := io.ReadFull(c.conn, make([]byte, 1)); err == nil {
		t.Fatal("expected the connection to close")
	}
}

func TestRFBBlackFrameWhenNothingEverDecodes(t *testing.T) {
	old := rfbFirstFrameWait
	rfbFirstFrameWait = 100 * time.Millisecond
	t.Cleanup(func() { rfbFirstFrameWait = old })
	srv, v := rfbFixture(t)
	v.InputState([]byte(`{"ready":true,"width":8,"height":4,"fps":30}`))
	c := dialRFB(t, srv, []string{"binary"})
	if w, h := c.handshake(); w != 8 || h != 4 {
		t.Fatalf("ServerInit %dx%d, want the videoInputState size 8x4", w, h)
	}
	c.request(false, 8, 4)
	rs := c.update()
	if len(rs) != 1 || rs[0].w != 8 || rs[0].h != 4 {
		t.Fatalf("rects %+v", rs)
	}
	for _, b := range rs[0].pix {
		if b != 0 {
			t.Fatal("expected a black frame")
		}
	}
	// The real frame still reaches the client once it exists.
	c.request(true, 8, 4)
	v.Store().Set(8, 4, solid(8, 4, 7, 7, 7))
	if rs := c.update(); rs[0].pix[0] != 7 {
		t.Fatal("real frame not delivered after the black one")
	}
}

func TestRFBIgnoresInputMessages(t *testing.T) {
	srv, v := rfbFixture(t)
	v.Store().Set(2, 2, solid(2, 2, 1, 1, 1))
	c := dialRFB(t, srv, []string{"binary"})
	c.handshake()
	c.write(4, 1, 0, 0, 0, 0, 0xff, 0x0d)     // KeyEvent
	c.write(5, 1, 0, 3, 0, 4)                 // PointerEvent
	c.write(6, 0, 0, 0, 0, 0, 0, 2, 'h', 'i') // ClientCutText "hi"
	c.request(false, 2, 2)
	if rs := c.update(); len(rs) != 1 {
		t.Fatalf("rects %+v", rs)
	}
}

func TestRFBUnavailableWithoutFFmpeg(t *testing.T) {
	o := testOwner(t, func(context.Context) (rpcLink, error) { return newFakeLink(), nil })
	o.SetVideo(NewVideo("", "ffmpeg not found", &fakeHost{}))
	srv := httptest.NewServer(newHandler(o, testToken, func() {}))
	t.Cleanup(srv.Close)
	url := "ws" + strings.TrimPrefix(srv.URL, "http") + "/rfb?token=" + testToken
	_, resp, err := websocket.Dial(context.Background(), url, nil)
	if err == nil || resp == nil || resp.StatusCode != 503 {
		t.Fatalf("err=%v resp=%v, want 503", err, resp)
	}
}

func TestStatusReportsVideo(t *testing.T) {
	srv, v := rfbFixture(t)
	v.Store().Set(6, 5, solid(6, 5, 1, 1, 1))
	_, body := getAuthed(t, srv.URL+"/status")
	for _, want := range []string{`"video":{`, `"available":true`, `"width":6`, `"height":5`, `"frames_decoded":1`} {
		if !strings.Contains(body, want) {
			t.Fatalf("status lacks %s: %s", want, body)
		}
	}
}

func getAuthed(t *testing.T, url string) (int, string) {
	t.Helper()
	req, _ := http.NewRequest(http.MethodGet, url, nil)
	req.Header.Set("Authorization", "Bearer "+testToken)
	resp, err := http.DefaultClient.Do(req)
	if err != nil {
		t.Fatal(err)
	}
	defer resp.Body.Close()
	b, _ := io.ReadAll(resp.Body)
	return resp.StatusCode, string(b)
}
