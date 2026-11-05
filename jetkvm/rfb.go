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
	"bufio"
	"context"
	"encoding/binary"
	"errors"
	"fmt"
	"io"
	"log/slog"
	"net/http"
	"sync"
	"time"

	"github.com/coder/websocket"
)

// RFB 3.8 server over a WebSocket (GET /rfb). The WebSocket carries the RFB
// byte stream in binary messages of any size; message boundaries mean
// nothing. Only Raw (0) rectangles are sent, plus the DesktopSize (-223)
// pseudo-encoding when the client advertised it. Keyboard, pointer and
// clipboard messages are read and ignored: input goes through the hid path.

const (
	encRaw         = 0
	encDesktopSize = -223

	rfbName = "jetkvm"
	// maxCutText bounds a ClientCutText the server is willing to skip.
	maxCutText = 1 << 20
)

// rfbFirstFrameWait is how long a request waits for the first decoded frame
// before a black frame goes out instead.
var rfbFirstFrameWait = 10 * time.Second

type pixelFormat struct {
	bpp, depth             uint8
	bigEndian, trueColor   bool
	rMax, gMax, bMax       uint16
	rShift, gShift, bShift uint8
}

var nativeFormat = pixelFormat{
	bpp: 32, depth: 24, trueColor: true,
	rMax: 255, gMax: 255, bMax: 255, rShift: 16, gShift: 8, bShift: 0,
}

func (p pixelFormat) marshal() []byte {
	b := make([]byte, 16)
	b[0], b[1] = p.bpp, p.depth
	if p.bigEndian {
		b[2] = 1
	}
	if p.trueColor {
		b[3] = 1
	}
	binary.BigEndian.PutUint16(b[4:], p.rMax)
	binary.BigEndian.PutUint16(b[6:], p.gMax)
	binary.BigEndian.PutUint16(b[8:], p.bMax)
	b[10], b[11], b[12] = p.rShift, p.gShift, p.bShift
	return b
}

func parsePixelFormat(b []byte) pixelFormat {
	return pixelFormat{
		bpp: b[0], depth: b[1], bigEndian: b[2] != 0, trueColor: b[3] != 0,
		rMax: binary.BigEndian.Uint16(b[4:]), gMax: binary.BigEndian.Uint16(b[6:]),
		bMax:   binary.BigEndian.Uint16(b[8:]),
		rShift: b[10], gShift: b[11], bShift: b[12],
	}
}

// convertRow converts n bgr0 pixels from src into dst (4*n bytes) in pf.
func convertRow(dst, src []byte, n int, pf pixelFormat) {
	if pf == nativeFormat {
		copy(dst, src[:4*n])
		return
	}
	for i := 0; i < n; i++ {
		b, g, r := uint32(src[4*i]), uint32(src[4*i+1]), uint32(src[4*i+2])
		v := (r*uint32(pf.rMax)/255)<<pf.rShift |
			(g*uint32(pf.gMax)/255)<<pf.gShift |
			(b*uint32(pf.bMax)/255)<<pf.bShift
		if pf.bigEndian {
			binary.BigEndian.PutUint32(dst[4*i:], v)
		} else {
			binary.LittleEndian.PutUint32(dst[4*i:], v)
		}
	}
}

// serveRFB is the GET /rfb handler.
func serveRFB(v *Video, w http.ResponseWriter, r *http.Request) {
	if v == nil {
		http.Error(w, "video is not enabled", http.StatusServiceUnavailable)
		return
	}
	if err := v.Acquire(); err != nil {
		http.Error(w, "video unavailable: "+err.Error(), http.StatusServiceUnavailable)
		return
	}
	defer v.Release()
	// The auth layer has already checked Host, Origin and token.
	c, err := websocket.Accept(w, r, &websocket.AcceptOptions{
		InsecureSkipVerify: true,
		Subprotocols:       []string{"binary"},
	})
	if err != nil {
		return
	}
	defer c.CloseNow()
	c.SetReadLimit(maxCutText + 64)
	ctx, cancel := context.WithCancel(r.Context())
	defer cancel()
	conn := websocket.NetConn(ctx, c, websocket.MessageBinary)
	s := &rfbSession{
		v: v, ctx: ctx, cancel: cancel,
		br: bufio.NewReader(conn), bw: bufio.NewWriterSize(conn, 64<<10),
		pf: nativeFormat, wake: make(chan struct{}, 1),
	}
	if err := s.run(); err != nil && ctx.Err() == nil && !errors.Is(err, io.EOF) {
		slog.Warn("rfb client ended", "err", err)
	}
}

type fbRequest struct {
	have, incremental bool
}

type rfbSession struct {
	v      *Video
	ctx    context.Context
	cancel context.CancelFunc
	br     *bufio.Reader
	bw     *bufio.Writer

	mu          sync.Mutex // guards the fields below (set by the reader)
	pf          pixelFormat
	desktopSize bool
	pending     fbRequest
	wake        chan struct{}

	cw, ch  int    // the size the client believes the framebuffer has
	lastSeq uint64 // newest frame sent; 0 forces the next reply
}

func (s *rfbSession) run() error {
	if err := s.handshake(); err != nil {
		return err
	}
	errc := make(chan error, 1)
	go func() {
		err := s.readLoop()
		errc <- err
		s.cancel()
	}()
	err := s.respondLoop()
	select {
	case rerr := <-errc:
		if err == nil || errors.Is(err, context.Canceled) {
			return rerr
		}
	default:
	}
	return err
}

func (s *rfbSession) handshake() error {
	if _, err := s.bw.WriteString("RFB 003.008\n"); err != nil {
		return err
	}
	if err := s.bw.Flush(); err != nil {
		return err
	}
	ver := make([]byte, 12)
	if _, err := io.ReadFull(s.br, ver); err != nil {
		return err
	}
	minor := 0
	switch string(ver) {
	case "RFB 003.008\n":
		minor = 8
	case "RFB 003.007\n":
		minor = 7
	case "RFB 003.003\n":
		minor = 3
	default:
		return fmt.Errorf("unsupported protocol version %q", ver)
	}
	if minor == 3 {
		_ = binary.Write(s.bw, binary.BigEndian, uint32(1)) // security type None
	} else {
		if _, err := s.bw.Write([]byte{1, 1}); err != nil { // one type: None
			return err
		}
		if err := s.bw.Flush(); err != nil {
			return err
		}
		sel, err := s.br.ReadByte()
		if err != nil {
			return err
		}
		if sel != 1 {
			return fmt.Errorf("client chose security type %d", sel)
		}
		if minor == 8 {
			_ = binary.Write(s.bw, binary.BigEndian, uint32(0)) // SecurityResult OK
		}
	}
	if err := s.bw.Flush(); err != nil {
		return err
	}
	if _, err := s.br.ReadByte(); err != nil { // ClientInit: shared flag
		return err
	}
	s.cw, s.ch = s.v.KnownSize()
	init := make([]byte, 4, 4+16+4+len(rfbName))
	binary.BigEndian.PutUint16(init[0:], uint16(s.cw))
	binary.BigEndian.PutUint16(init[2:], uint16(s.ch))
	init = append(init, nativeFormat.marshal()...)
	init = binary.BigEndian.AppendUint32(init, uint32(len(rfbName)))
	init = append(init, rfbName...)
	if _, err := s.bw.Write(init); err != nil {
		return err
	}
	return s.bw.Flush()
}

func (s *rfbSession) readLoop() error {
	for {
		t, err := s.br.ReadByte()
		if err != nil {
			return err
		}
		switch t {
		case 0: // SetPixelFormat
			b := make([]byte, 19)
			if _, err := io.ReadFull(s.br, b); err != nil {
				return err
			}
			pf := parsePixelFormat(b[3:])
			if pf.bpp != 32 || !pf.trueColor || pf.depth > 32 ||
				pf.rShift > 31 || pf.gShift > 31 || pf.bShift > 31 {
				return fmt.Errorf("unsupported pixel format (only 32bpp true-color): %+v", pf)
			}
			s.mu.Lock()
			s.pf = pf
			s.mu.Unlock()
		case 2: // SetEncodings
			b := make([]byte, 3)
			if _, err := io.ReadFull(s.br, b); err != nil {
				return err
			}
			n := int(binary.BigEndian.Uint16(b[1:]))
			encs := make([]byte, 4*n)
			if _, err := io.ReadFull(s.br, encs); err != nil {
				return err
			}
			ds := false
			for i := 0; i < n; i++ {
				if int32(binary.BigEndian.Uint32(encs[4*i:])) == encDesktopSize {
					ds = true
				}
			}
			s.mu.Lock()
			s.desktopSize = ds
			s.mu.Unlock()
		case 3: // FramebufferUpdateRequest
			b := make([]byte, 9)
			if _, err := io.ReadFull(s.br, b); err != nil {
				return err
			}
			s.mu.Lock()
			// A non-incremental request outranks an incremental one.
			s.pending = fbRequest{have: true, incremental: b[0] != 0 && (!s.pending.have || s.pending.incremental)}
			s.mu.Unlock()
			select {
			case s.wake <- struct{}{}:
			default:
			}
		case 4: // KeyEvent
			if _, err := io.CopyN(io.Discard, s.br, 7); err != nil {
				return err
			}
		case 5: // PointerEvent
			if _, err := io.CopyN(io.Discard, s.br, 5); err != nil {
				return err
			}
		case 6: // ClientCutText
			b := make([]byte, 7)
			if _, err := io.ReadFull(s.br, b); err != nil {
				return err
			}
			n := int64(binary.BigEndian.Uint32(b[3:]))
			if n > maxCutText {
				return errors.New("client cut text too large")
			}
			if _, err := io.CopyN(io.Discard, s.br, n); err != nil {
				return err
			}
		default:
			return fmt.Errorf("unknown client message type %d", t)
		}
	}
}

func (s *rfbSession) respondLoop() error {
	for {
		select {
		case <-s.wake:
		case <-s.ctx.Done():
			return s.ctx.Err()
		}
		s.mu.Lock()
		req := s.pending
		s.pending = fbRequest{}
		s.mu.Unlock()
		if !req.have {
			continue
		}
		f, err := s.pick(req)
		if err != nil {
			return err
		}
		if err := s.sendFrame(f); err != nil {
			return err
		}
	}
}

// pick chooses the frame that answers req, blocking when nothing newer exists.
func (s *rfbSession) pick(req fbRequest) (Frame, error) {
	store := s.v.Store()
	cur := store.Latest()
	if cur.Seq > 0 && (!req.incremental || cur.Seq > s.lastSeq) {
		return cur, nil
	}
	if cur.Seq == 0 {
		// No frame has ever decoded: do not hang the client forever.
		wctx, cancel := context.WithTimeout(s.ctx, rfbFirstFrameWait)
		defer cancel()
		f, err := store.Wait(wctx, 0)
		if err == nil {
			return f, nil
		}
		if s.ctx.Err() != nil {
			return Frame{}, s.ctx.Err()
		}
		w, h := s.v.KnownSize()
		return Frame{Seq: 0, W: w, H: h, Pix: make([]byte, w*h*4)}, nil
	}
	return store.Wait(s.ctx, s.lastSeq)
}

func (s *rfbSession) sendFrame(f Frame) error {
	s.mu.Lock()
	pf, ds := s.pf, s.desktopSize
	s.mu.Unlock()
	rects := 1
	resize := f.W != s.cw || f.H != s.ch
	if resize {
		if !ds {
			return fmt.Errorf("picture size changed to %dx%d and the client did not "+
				"advertise DesktopSize; reconnect", f.W, f.H)
		}
		rects = 2
	}
	hdr := []byte{0, 0, 0, byte(rects)}
	if _, err := s.bw.Write(hdr); err != nil {
		return err
	}
	rect := func(w, h int, enc int32) error {
		b := make([]byte, 12)
		binary.BigEndian.PutUint16(b[4:], uint16(w))
		binary.BigEndian.PutUint16(b[6:], uint16(h))
		binary.BigEndian.PutUint32(b[8:], uint32(enc))
		_, err := s.bw.Write(b)
		return err
	}
	if resize {
		if err := rect(f.W, f.H, encDesktopSize); err != nil {
			return err
		}
		s.cw, s.ch = f.W, f.H
	}
	if err := rect(f.W, f.H, encRaw); err != nil {
		return err
	}
	row := make([]byte, 4*f.W)
	for y := 0; y < f.H; y++ {
		convertRow(row, f.Pix[y*4*f.W:], f.W, pf)
		if _, err := s.bw.Write(row); err != nil {
			return err
		}
	}
	s.lastSeq = f.Seq
	return s.bw.Flush()
}
