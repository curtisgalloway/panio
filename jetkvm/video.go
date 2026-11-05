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
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"log/slog"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"sync"
	"sync/atomic"
	"time"

	"github.com/pion/rtp"
	"github.com/pion/rtp/codecs"
	"github.com/pion/webrtc/v4/pkg/media/samplebuilder"
)

// Video turns the device's H.264 RTP track into decoded frames. The track is
// part of the one session the Owner already holds. Decoding runs only while at
// least one /rfb client is attached (Acquire/Release); RTP that arrives while
// idle is read and discarded.
//
// ffmpeg is run without -fflags nobuffer: with it a raw H.264 pipe produces
// no output at all (measured with ffmpeg 8).
//
// Pipeline: RTP -> samplebuilder/H264Packet (Annex-B access units) -> an
// ffmpeg subprocess (-f h264 on stdin, raw bgr0 on stdout) -> FrameStore.

const (
	// keyframeEvery is how often a PLI is repeated until a frame decodes.
	keyframeEvery = 3 * time.Second
	// defaultLinger is how long decoding continues after the last client left.
	defaultLinger = 10 * time.Second
	// maxFrameBytes bounds one decoded frame (8K at 4 bytes per pixel).
	maxFrameBytes = 7680 * 4320 * 4
)

// videoHost is what Video needs from the Owner.
type videoHost interface {
	// ensure connects the session if it is not up.
	ensure() error
	// linkState reports whether a session is up and whether it was kicked.
	linkState() (connected, kicked bool)
}

// Frame is one decoded picture in bgr0 order (blue, green, red, pad).
type Frame struct {
	Seq  uint64
	W, H int
	Pix  []byte // never modified after it is stored
}

// FrameStore keeps only the newest frame.
type FrameStore struct {
	mu     sync.Mutex
	cur    Frame
	notify chan struct{}
	count  atomic.Uint64
	lastAt atomic.Int64 // unix nanos of the newest frame
}

// NewFrameStore returns an empty store.
func NewFrameStore() *FrameStore { return &FrameStore{notify: make(chan struct{})} }

// Set stores pix (the store takes ownership) as the newest frame.
func (s *FrameStore) Set(w, h int, pix []byte) uint64 {
	s.mu.Lock()
	s.cur = Frame{Seq: s.cur.Seq + 1, W: w, H: h, Pix: pix}
	seq := s.cur.Seq
	close(s.notify)
	s.notify = make(chan struct{})
	s.mu.Unlock()
	s.count.Add(1)
	s.lastAt.Store(time.Now().UnixNano())
	return seq
}

// Latest returns the newest frame; Seq is 0 when there is none yet.
func (s *FrameStore) Latest() Frame {
	s.mu.Lock()
	defer s.mu.Unlock()
	return s.cur
}

// Wait blocks until a frame with Seq > after exists, then returns it.
func (s *FrameStore) Wait(ctx context.Context, after uint64) (Frame, error) {
	for {
		s.mu.Lock()
		f, ch := s.cur, s.notify
		s.mu.Unlock()
		if f.Seq > after {
			return f, nil
		}
		select {
		case <-ch:
		case <-ctx.Done():
			return Frame{}, ctx.Err()
		}
	}
}

// findFFmpeg resolves the ffmpeg binary: the explicit path, else PATH, else
// the usual install locations.
func findFFmpeg(explicit string) (string, error) {
	if explicit != "" {
		if _, err := os.Stat(explicit); err != nil {
			return "", fmt.Errorf("ffmpeg not found at %s", explicit)
		}
		return explicit, nil
	}
	if p, err := exec.LookPath("ffmpeg"); err == nil {
		return p, nil
	}
	for _, dir := range []string{"/opt/homebrew/bin", "/usr/local/bin", "/usr/bin"} {
		p := filepath.Join(dir, "ffmpeg")
		if st, err := os.Stat(p); err == nil && !st.IsDir() {
			return p, nil
		}
	}
	return "", errors.New("ffmpeg not found (install it, or pass --ffmpeg PATH); " +
		"video is unavailable, HID still works")
}

// Video is the video pipeline for one daemon.
type Video struct {
	ffmpeg string // "" means unavailable
	store  *FrameStore
	host   videoHost
	linger time.Duration

	mu         sync.Mutex
	refs       int
	active     bool
	stopTimer  *time.Timer
	keyframe   func()
	gotFrame   bool // a frame decoded since the last attach
	sb         *samplebuilder.SampleBuilder
	dec        *decoder
	sps, pps   []byte
	spsW, spsH int
	inW, inH   int // from videoInputState
	lastErr    string
	cancel     context.CancelFunc
	closed     bool
}

// NewVideo returns a Video. ffmpeg is the resolved binary path, or "" with
// unavailable set to the reason.
func NewVideo(ffmpeg, unavailable string, host videoHost) *Video {
	v := &Video{ffmpeg: ffmpeg, store: NewFrameStore(), host: host, linger: defaultLinger}
	if ffmpeg == "" {
		v.lastErr = unavailable
	}
	return v
}

// Available reports whether decoding can run, and why not when it cannot.
func (v *Video) Available() (bool, string) {
	v.mu.Lock()
	defer v.mu.Unlock()
	if v.ffmpeg == "" {
		return false, v.lastErr
	}
	return true, ""
}

// Store exposes the frame store.
func (v *Video) Store() *FrameStore { return v.store }

// KnownSize is the best guess at the picture size before/without a frame.
func (v *Video) KnownSize() (int, int) {
	if f := v.store.Latest(); f.Seq > 0 {
		return f.W, f.H
	}
	v.mu.Lock()
	defer v.mu.Unlock()
	switch {
	case v.spsW > 0:
		return v.spsW, v.spsH
	case v.inW > 0:
		return v.inW, v.inH
	}
	return 1920, 1080
}

// Acquire registers an /rfb client and starts the video path if needed.
func (v *Video) Acquire() error {
	if ok, why := v.Available(); !ok {
		return errors.New(why)
	}
	v.mu.Lock()
	defer v.mu.Unlock()
	if v.closed {
		return errors.New("video is shutting down")
	}
	v.refs++
	if v.stopTimer != nil {
		v.stopTimer.Stop()
		v.stopTimer = nil
	}
	if !v.active {
		v.active = true
		v.gotFrame = false
		v.sb = newSampleBuilder()
		ctx, cancel := context.WithCancel(context.Background())
		v.cancel = cancel
		go v.keeper(ctx)
	}
	return nil
}

// Release drops an /rfb client; decoding stops after the linger period.
func (v *Video) Release() {
	v.mu.Lock()
	defer v.mu.Unlock()
	if v.refs > 0 {
		v.refs--
	}
	if v.refs == 0 && v.active && v.stopTimer == nil {
		v.stopTimer = time.AfterFunc(v.linger, v.deactivate)
	}
}

func (v *Video) deactivate() {
	v.mu.Lock()
	if v.refs > 0 || !v.active {
		v.mu.Unlock()
		return
	}
	v.active = false
	v.stopTimer = nil
	if v.cancel != nil {
		v.cancel()
	}
	d := v.dec
	v.dec = nil
	v.mu.Unlock()
	if d != nil {
		d.stop()
	}
}

// Close stops everything; ffmpeg is killed and reaped before it returns.
func (v *Video) Close() {
	v.mu.Lock()
	v.closed = true
	v.refs = 0
	v.active = false
	if v.stopTimer != nil {
		v.stopTimer.Stop()
		v.stopTimer = nil
	}
	if v.cancel != nil {
		v.cancel()
	}
	d := v.dec
	v.dec = nil
	v.mu.Unlock()
	if d != nil {
		d.stop()
	}
}

// keeper connects the session on demand and asks for keyframes until one
// decodes.
func (v *Video) keeper(ctx context.Context) {
	first := true
	t := time.NewTicker(keyframeEvery)
	defer t.Stop()
	for {
		connected, kicked := v.host.linkState()
		switch {
		case !connected && (first || !kicked):
			// The first attempt is explicit intent (a client just attached);
			// later ones must not fight a person's browser for the session.
			if err := v.host.ensure(); err != nil {
				v.setErr("JetKVM session: " + err.Error())
			} else {
				v.setErr("")
			}
		case !connected && kicked:
			v.setErr("session taken over by another client; reattach to retry")
		}
		first = false
		v.mu.Lock()
		kf, need := v.keyframe, !v.gotFrame
		v.mu.Unlock()
		if kf != nil && need {
			kf()
		}
		select {
		case <-ctx.Done():
			return
		case <-t.C:
		}
	}
}

func (v *Video) setErr(s string) {
	v.mu.Lock()
	v.lastErr = s
	v.mu.Unlock()
}

// Attach is called by the Link when its video track arrives (a new session,
// so a new SSRC and sequence space).
func (v *Video) Attach(keyframe func()) {
	v.mu.Lock()
	v.keyframe = keyframe
	v.gotFrame = false
	v.sb = newSampleBuilder()
	active := v.active
	v.mu.Unlock()
	if active && keyframe != nil {
		keyframe()
	}
}

// InputState handles the device's videoInputState event.
func (v *Video) InputState(params json.RawMessage) {
	var st struct {
		Ready  bool `json:"ready"`
		Width  int  `json:"width"`
		Height int  `json:"height"`
	}
	if json.Unmarshal(params, &st) != nil {
		return
	}
	v.mu.Lock()
	if st.Width > 0 && st.Height > 0 {
		v.inW, v.inH = st.Width, st.Height
	}
	v.mu.Unlock()
}

func newSampleBuilder() *samplebuilder.SampleBuilder {
	return samplebuilder.New(128, &codecs.H264Packet{}, 90000)
}

// Packet takes one RTP packet from the track.
func (v *Video) Packet(pkt *rtp.Packet) {
	v.mu.Lock()
	if !v.active || v.sb == nil {
		v.mu.Unlock()
		return
	}
	v.sb.Push(pkt)
	var aus [][]byte
	for {
		s := v.sb.Pop()
		if s == nil {
			break
		}
		aus = append(aus, s.Data)
	}
	v.mu.Unlock()
	for _, au := range aus {
		v.feedAU(au)
	}
}

// feedAU takes one Annex-B access unit.
func (v *Video) feedAU(au []byte) {
	nals := splitAnnexB(au)
	var sps, pps []byte
	for _, n := range nals {
		switch n[0] & 0x1f {
		case 7:
			sps = n
		case 8:
			pps = n
		}
	}
	v.mu.Lock()
	if !v.active {
		v.mu.Unlock()
		return
	}
	if sps != nil {
		if w, h, err := parseSPS(sps); err == nil {
			v.sps, v.spsW, v.spsH = append([]byte(nil), sps...), w, h
		} else {
			slog.Warn("unparseable SPS", "err", err)
		}
	}
	if pps != nil {
		v.pps = append([]byte(nil), pps...)
	}
	if v.spsW == 0 {
		v.mu.Unlock()
		return // nothing can be decoded before the first SPS
	}
	var old *decoder
	if v.dec == nil || v.dec.w != v.spsW || v.dec.h != v.spsH {
		old = v.dec
		nd, err := startDecoder(v.ffmpeg, v.spsW, v.spsH, v.store, v.onFrame, v.onDecoderExit)
		if err != nil {
			v.dec = nil
			v.lastErr = "ffmpeg: " + err.Error()
			v.mu.Unlock()
			if old != nil {
				old.stop()
			}
			return
		}
		v.dec = nd
		if sps == nil || pps == nil { // join mid-stream: prime from the cache
			nd.write(startCode(v.sps))
			nd.write(startCode(v.pps))
		}
	}
	d := v.dec
	v.mu.Unlock()
	if old != nil {
		old.stop()
	}
	// A trailing access-unit delimiter lets ffmpeg's parser see the end of this
	// access unit now instead of when the next one starts (one frame of lag).
	if !d.write(append(au[:len(au):len(au)], 0, 0, 0, 1, 0x09, 0x10)) {
		slog.Warn("video decoder is behind; dropped an access unit")
		v.mu.Lock()
		kf := v.keyframe
		v.mu.Unlock()
		if kf != nil {
			kf()
		}
	}
}

func startCode(nal []byte) []byte {
	if nal == nil {
		return nil
	}
	return append([]byte{0, 0, 0, 1}, nal...)
}

func (v *Video) onFrame() {
	v.mu.Lock()
	v.gotFrame = true
	v.lastErr = ""
	v.mu.Unlock()
}

func (v *Video) onDecoderExit(d *decoder, err error) {
	v.mu.Lock()
	defer v.mu.Unlock()
	if v.dec == d {
		v.dec = nil
		if err != nil {
			v.lastErr = "ffmpeg exited: " + err.Error()
		}
	}
}

// VideoStatus is the /status "video" object.
type VideoStatus struct {
	Available      bool   `json:"available"`
	Decoding       bool   `json:"decoding"`
	Width          int    `json:"width"`
	Height         int    `json:"height"`
	FramesDecoded  uint64 `json:"frames_decoded"`
	LastFrameAgeMs int64  `json:"last_frame_age_ms"` // -1 when no frame yet
	Error          string `json:"error"`
}

// Status describes the pipeline.
func (v *Video) Status() VideoStatus {
	if v == nil {
		return VideoStatus{LastFrameAgeMs: -1, Error: "video is not enabled"}
	}
	v.mu.Lock()
	st := VideoStatus{
		Available: v.ffmpeg != "",
		Decoding:  v.dec != nil,
		Error:     v.lastErr,
	}
	w, h := v.inW, v.inH
	v.mu.Unlock()
	f := v.store.Latest()
	if f.Seq > 0 {
		w, h = f.W, f.H
	}
	st.Width, st.Height = w, h
	st.FramesDecoded = v.store.count.Load()
	st.LastFrameAgeMs = -1
	if at := v.store.lastAt.Load(); at != 0 {
		st.LastFrameAgeMs = time.Since(time.Unix(0, at)).Milliseconds()
	}
	return st
}

// ---- ffmpeg subprocess ----

type decoder struct {
	w, h  int
	cmd   *exec.Cmd
	stdin io.WriteCloser
	in    chan []byte
	quit  chan struct{}
	done  chan struct{} // closed when the process is reaped
	once  sync.Once
	pid   int
}

func startDecoder(ffmpeg string, w, h int, store *FrameStore, onFrame func(),
	onExit func(*decoder, error)) (*decoder, error) {
	if w <= 0 || h <= 0 || w*h*4 > maxFrameBytes {
		return nil, fmt.Errorf("unsupported picture size %dx%d", w, h)
	}
	cmd := exec.Command(ffmpeg, "-hide_banner", "-loglevel", "error",
		"-flags", "low_delay", "-probesize", "32", "-analyzeduration", "0",
		"-f", "h264", "-i", "pipe:0",
		"-f", "rawvideo", "-pix_fmt", "bgr0", "pipe:1")
	stdin, err := cmd.StdinPipe()
	if err != nil {
		return nil, err
	}
	stdout, err := cmd.StdoutPipe()
	if err != nil {
		return nil, err
	}
	var stderr bytes.Buffer
	cmd.Stderr = &limitedWriter{w: &stderr, max: 4096}
	if err := cmd.Start(); err != nil {
		return nil, err
	}
	d := &decoder{w: w, h: h, cmd: cmd, stdin: stdin, in: make(chan []byte, 128),
		quit: make(chan struct{}), done: make(chan struct{}), pid: cmd.Process.Pid}
	// Writer: serializes access units onto ffmpeg's stdin.
	go func() {
		for {
			select {
			case b := <-d.in:
				if _, err := stdin.Write(b); err != nil {
					return
				}
			case <-d.quit:
				return
			}
		}
	}()
	// Reader: whole frames from stdout into the store.
	go func() {
		size := w * h * 4
		for {
			buf := make([]byte, size)
			if _, err := io.ReadFull(stdout, buf); err != nil {
				break
			}
			select {
			case <-d.quit:
				goto reap
			default:
			}
			store.Set(w, h, buf)
			onFrame()
		}
	reap:
		err := cmd.Wait()
		close(d.done)
		select {
		case <-d.quit:
			onExit(d, nil)
		default:
			msg := strings.TrimSpace(stderr.String())
			if msg == "" && err != nil {
				msg = err.Error()
			}
			if msg == "" {
				msg = "stream ended"
			}
			onExit(d, errors.New(msg))
		}
	}()
	return d, nil
}

// write queues b; false means the decoder is too far behind and b was dropped.
func (d *decoder) write(b []byte) bool {
	if b == nil {
		return true
	}
	select {
	case d.in <- b:
		return true
	default:
		return false
	}
}

// stop kills ffmpeg and waits until it is reaped.
func (d *decoder) stop() {
	d.once.Do(func() {
		close(d.quit)
		_ = d.stdin.Close()
		if d.cmd.Process != nil {
			_ = d.cmd.Process.Kill()
		}
	})
	select {
	case <-d.done:
	case <-time.After(5 * time.Second):
	}
}

type limitedWriter struct {
	w   *bytes.Buffer
	max int
	mu  sync.Mutex
}

func (l *limitedWriter) Write(p []byte) (int, error) {
	l.mu.Lock()
	defer l.mu.Unlock()
	if room := l.max - l.w.Len(); room > 0 {
		if len(p) < room {
			room = len(p)
		}
		l.w.Write(p[:room])
	}
	return len(p), nil
}

// ---- Annex-B and SPS parsing ----

// splitAnnexB returns the NAL units (without start codes) in b.
func splitAnnexB(b []byte) [][]byte {
	var out [][]byte
	start := -1
	for i := 0; i+2 < len(b); i++ {
		if b[i] == 0 && b[i+1] == 0 && b[i+2] == 1 {
			if start >= 0 {
				out = append(out, trimZeros(b[start:i]))
			}
			start = i + 3
			i += 2
		}
	}
	if start >= 0 && start < len(b) {
		out = append(out, trimZeros(b[start:]))
	}
	res := out[:0]
	for _, n := range out {
		if len(n) > 0 {
			res = append(res, n)
		}
	}
	return res
}

func trimZeros(b []byte) []byte {
	for len(b) > 0 && b[len(b)-1] == 0 {
		b = b[:len(b)-1]
	}
	return b
}

type bitReader struct {
	b   []byte
	pos int
	err error
}

func (r *bitReader) bit() uint {
	if r.pos>>3 >= len(r.b) {
		r.err = errors.New("SPS truncated")
		return 0
	}
	v := uint(r.b[r.pos>>3]>>(7-uint(r.pos&7))) & 1
	r.pos++
	return v
}

func (r *bitReader) bits(n int) uint {
	var v uint
	for i := 0; i < n; i++ {
		v = v<<1 | r.bit()
	}
	return v
}

func (r *bitReader) ue() uint {
	zeros := 0
	for r.bit() == 0 && r.err == nil {
		zeros++
		if zeros > 32 {
			r.err = errors.New("bad exp-Golomb code")
			return 0
		}
	}
	return (1 << uint(zeros)) - 1 + r.bits(zeros)
}

func (r *bitReader) se() int {
	k := r.ue()
	if k&1 == 1 {
		return int((k + 1) / 2)
	}
	return -int(k / 2)
}

// parseSPS returns the cropped picture size of an H.264 SPS NAL (header byte
// included).
func parseSPS(nal []byte) (w, h int, err error) {
	if len(nal) < 4 || nal[0]&0x1f != 7 {
		return 0, 0, errors.New("not an SPS")
	}
	// Remove emulation-prevention bytes.
	rbsp := make([]byte, 0, len(nal))
	zeros := 0
	for _, c := range nal[1:] {
		if zeros >= 2 && c == 3 {
			zeros = 0
			continue
		}
		if c == 0 {
			zeros++
		} else {
			zeros = 0
		}
		rbsp = append(rbsp, c)
	}
	r := &bitReader{b: rbsp}
	profile := r.bits(8)
	r.bits(8) // constraint flags + reserved
	r.bits(8) // level
	r.ue()    // sps id
	chroma := uint(1)
	switch profile {
	case 100, 110, 122, 244, 44, 83, 86, 118, 128, 138, 139, 134, 135:
		chroma = r.ue()
		if chroma == 3 {
			r.bit()
		}
		r.ue()
		r.ue()
		r.bit()
		if r.bit() == 1 {
			lists := 8
			if chroma == 3 {
				lists = 12
			}
			for i := 0; i < lists; i++ {
				if r.bit() == 1 {
					size := 16
					if i >= 6 {
						size = 64
					}
					last, next := 8, 8
					for j := 0; j < size && r.err == nil; j++ {
						if next != 0 {
							next = (last + r.se() + 256) % 256
						}
						if next != 0 {
							last = next
						}
					}
				}
			}
		}
	}
	r.ue() // log2_max_frame_num
	switch r.ue() {
	case 0:
		r.ue()
	case 1:
		r.bit()
		r.se()
		r.se()
		n := r.ue()
		for i := uint(0); i < n && r.err == nil; i++ {
			r.se()
		}
	}
	r.ue() // max_num_ref_frames
	r.bit()
	mbW := int(r.ue()) + 1
	mapH := int(r.ue()) + 1
	frameMbsOnly := int(r.bit())
	if frameMbsOnly == 0 {
		r.bit()
	}
	r.bit()
	w, h = mbW*16, (2-frameMbsOnly)*mapH*16
	if r.bit() == 1 {
		l, rt, t, b := int(r.ue()), int(r.ue()), int(r.ue()), int(r.ue())
		cux, cuy := 1, 2-frameMbsOnly
		if chroma != 0 {
			subW, subH := 2, 2
			if chroma == 2 {
				subH = 1
			}
			if chroma == 3 {
				subW, subH = 1, 1
			}
			cux, cuy = subW, subH*(2-frameMbsOnly)
		}
		w -= (l + rt) * cux
		h -= (t + b) * cuy
	}
	if r.err != nil {
		return 0, 0, r.err
	}
	if w <= 0 || h <= 0 {
		return 0, 0, fmt.Errorf("implausible picture size %dx%d", w, h)
	}
	return w, h, nil
}
