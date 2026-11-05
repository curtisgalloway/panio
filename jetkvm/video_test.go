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
	"os/exec"
	"strconv"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/pion/rtp"
	"github.com/pion/rtp/codecs"
)

// fakeHost is a videoHost whose session is always up.
type fakeHost struct {
	mu      sync.Mutex
	ensured int
}

func (f *fakeHost) ensure() error {
	f.mu.Lock()
	f.ensured++
	f.mu.Unlock()
	return nil
}
func (f *fakeHost) linkState() (bool, bool) { return true, false }

func needFFmpeg(t *testing.T) string {
	t.Helper()
	p, err := findFFmpeg("")
	if err != nil {
		t.Skip("ffmpeg not available: ", err)
	}
	return p
}

// genClip renders a short Annex-B H.264 clip with ffmpeg.
func genClip(t *testing.T, ffmpeg, size string, rate, secs int) []byte {
	t.Helper()
	var out, errb bytes.Buffer
	cmd := exec.Command(ffmpeg, "-hide_banner", "-loglevel", "error",
		"-f", "lavfi", "-i", "testsrc=size="+size+":rate="+itoa(rate),
		"-t", itoa(secs), "-pix_fmt", "yuv420p", "-c:v", "libx264", "-preset", "ultrafast",
		"-g", "5", "-bsf:v", "h264_mp4toannexb", "-f", "h264", "-")
	cmd.Stdout, cmd.Stderr = &out, &errb
	if err := cmd.Run(); err != nil {
		t.Fatalf("generating clip: %v: %s", err, errb.String())
	}
	return out.Bytes()
}

func itoa(n int) string { return strconv.Itoa(n) }

// accessUnits groups a single-slice-per-frame Annex-B stream into access
// units: leading parameter sets/SEI plus one slice.
func accessUnits(clip []byte) [][]byte {
	var aus [][]byte
	var cur []byte
	for _, n := range splitAnnexB(clip) {
		cur = append(cur, 0, 0, 0, 1)
		cur = append(cur, n...)
		if t := n[0] & 0x1f; t == 1 || t == 5 {
			aus = append(aus, cur)
			cur = nil
		}
	}
	return aus
}

// rtpStream packetizes access units into one continuous RTP stream (30 fps
// clock), so consecutive clips share sequence numbers and timestamps.
type rtpStream struct{ pk rtp.Packetizer }

func newRTPStream() *rtpStream {
	return &rtpStream{rtp.NewPacketizer(1200, 102, 0x1234, &codecs.H264Payloader{}, rtp.NewFixedSequencer(1), 90000)}
}

func (s *rtpStream) packetize(aus [][]byte) [][]*rtp.Packet {
	var out [][]*rtp.Packet
	for _, au := range aus {
		out = append(out, s.pk.Packetize(au, 3000))
	}
	return out
}

func packetize(aus [][]byte) [][]*rtp.Packet { return newRTPStream().packetize(aus) }

func feed(v *Video, frames [][]*rtp.Packet) {
	for _, f := range frames {
		for _, p := range f {
			v.Packet(p)
		}
	}
}

func waitFrames(t *testing.T, v *Video, n uint64) {
	t.Helper()
	waitFor(t, func() bool { return v.Status().FramesDecoded >= n }, "decoded frames")
}

// bitWriter builds an SPS RBSP for the parser test.
type bitWriter struct {
	b []byte
	n int
}

func (w *bitWriter) bit(v uint) {
	if w.n%8 == 0 {
		w.b = append(w.b, 0)
	}
	if v != 0 {
		w.b[len(w.b)-1] |= 1 << (7 - uint(w.n%8))
	}
	w.n++
}
func (w *bitWriter) bits(v uint, n int) {
	for i := n - 1; i >= 0; i-- {
		w.bit((v >> uint(i)) & 1)
	}
}
func (w *bitWriter) ue(v uint) {
	v++
	n := 0
	for t := v; t > 1; t >>= 1 {
		n++
	}
	w.bits(0, n)
	w.bits(v, n+1)
}

func TestParseSPSCropping(t *testing.T) {
	// High profile 1920x1080: 120x68 macroblocks, bottom crop of 4 units
	// (2 luma rows each for 4:2:0 frame_mbs_only) = 8 rows.
	w := &bitWriter{}
	w.bits(0x67&0x1f|0x60, 8) // nal header: ref_idc 3, type 7
	w.bits(100, 8)            // High
	w.bits(0, 8)
	w.bits(40, 8)
	w.ue(0) // sps id
	w.ue(1) // chroma 4:2:0
	w.ue(0)
	w.ue(0)
	w.bit(0)
	w.bit(0) // no scaling matrix
	w.ue(0)  // log2_max_frame_num-4
	w.ue(2)  // poc type 2
	w.ue(1)  // max refs
	w.bit(0)
	w.ue(119) // width in MBs - 1
	w.ue(67)  // height in map units - 1
	w.bit(1)  // frame_mbs_only
	w.bit(1)  // direct 8x8
	w.bit(1)  // cropping
	w.ue(0)
	w.ue(0)
	w.ue(0)
	w.ue(4)
	w.bit(0) // vui
	gw, gh, err := parseSPS(w.b)
	if err != nil || gw != 1920 || gh != 1080 {
		t.Fatalf("got %dx%d, %v; want 1920x1080", gw, gh, err)
	}
}

func TestParseSPSFromEncoder(t *testing.T) {
	ff := needFFmpeg(t)
	clip := genClip(t, ff, "202x102", 10, 1)
	for _, n := range splitAnnexB(clip) {
		if n[0]&0x1f == 7 {
			w, h, err := parseSPS(n)
			if err != nil || w != 202 || h != 102 {
				t.Fatalf("got %dx%d, %v; want 202x102", w, h, err)
			}
			return
		}
	}
	t.Fatal("no SPS in the clip")
}

func TestFrameStoreConcurrency(t *testing.T) {
	s := NewFrameStore()
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	var wg sync.WaitGroup
	for i := 0; i < 4; i++ {
		wg.Add(1)
		go func() {
			defer wg.Done()
			var last uint64
			for last < 200 {
				f, err := s.Wait(ctx, last)
				if err != nil {
					t.Error(err)
					return
				}
				if f.Seq <= last {
					t.Errorf("seq went backwards: %d after %d", f.Seq, last)
				}
				last = f.Seq
			}
		}()
	}
	for i := 0; i < 200; i++ {
		s.Set(2, 2, make([]byte, 16))
		time.Sleep(time.Millisecond)
	}
	wg.Wait()
	if s.Latest().Seq != 200 {
		t.Fatalf("seq = %d", s.Latest().Seq)
	}
}

func TestFFmpegPipelineEndToEnd(t *testing.T) {
	ff := needFFmpeg(t)
	clip := genClip(t, ff, "320x240", 10, 1)
	v := NewVideo(ff, "", &fakeHost{})
	if err := v.Acquire(); err != nil {
		t.Fatal(err)
	}
	defer v.Close()
	// Packets before Acquire would be dropped; these are decoded.
	feed(v, packetize(accessUnits(clip)))
	waitFrames(t, v, 6)
	f := v.Store().Latest()
	if f.W != 320 || f.H != 240 || len(f.Pix) != 320*240*4 {
		t.Fatalf("frame %dx%d, %d bytes", f.W, f.H, len(f.Pix))
	}
	if bytes.Count(f.Pix, f.Pix[:4]) == len(f.Pix)/4 {
		t.Fatal("decoded frame is one flat color; expected the test pattern")
	}
	st := v.Status()
	if !st.Available || !st.Decoding || st.Width != 320 || st.LastFrameAgeMs < 0 {
		t.Fatalf("status %+v", st)
	}
}

func TestResolutionChangeRestartsTheDecoder(t *testing.T) {
	ff := needFFmpeg(t)
	v := NewVideo(ff, "", &fakeHost{})
	if err := v.Acquire(); err != nil {
		t.Fatal(err)
	}
	defer v.Close()
	stream := newRTPStream()
	feed(v, stream.packetize(accessUnits(genClip(t, ff, "320x240", 10, 1))))
	waitFrames(t, v, 3)
	v.mu.Lock()
	first := v.dec
	v.mu.Unlock()
	feed(v, stream.packetize(accessUnits(genClip(t, ff, "160x120", 10, 1))))
	waitFor(t, func() bool { f := v.Store().Latest(); return f.W == 160 && f.H == 120 }, "160x120 frame")
	v.mu.Lock()
	second := v.dec
	v.mu.Unlock()
	if first == second {
		t.Fatal("decoder was not restarted on a resolution change")
	}
	select {
	case <-first.done:
	case <-time.After(3 * time.Second):
		t.Fatal("old ffmpeg was not reaped")
	}
}

func TestDecodingStopsAfterTheLastClientLeaves(t *testing.T) {
	ff := needFFmpeg(t)
	v := NewVideo(ff, "", &fakeHost{})
	v.linger = 100 * time.Millisecond
	if err := v.Acquire(); err != nil {
		t.Fatal(err)
	}
	feed(v, packetize(accessUnits(genClip(t, ff, "320x240", 10, 1))))
	waitFrames(t, v, 2)
	v.mu.Lock()
	d := v.dec
	v.mu.Unlock()
	v.Release()
	select {
	case <-d.done:
	case <-time.After(3 * time.Second):
		t.Fatal("ffmpeg still running after the linger period")
	}
	if v.Status().Decoding {
		t.Fatal("still reported as decoding")
	}
	// Idle packets are discarded, not decoded.
	n := v.Status().FramesDecoded
	feed(v, packetize(accessUnits(genClip(t, ff, "320x240", 10, 1))))
	time.Sleep(200 * time.Millisecond)
	if v.Status().FramesDecoded != n || v.Status().Decoding {
		t.Fatal("decoded while idle")
	}
}

func TestFFmpegMissingMakesVideoUnavailable(t *testing.T) {
	if _, err := findFFmpeg("/nonexistent/ffmpeg"); err == nil {
		t.Fatal("expected an error for a bad --ffmpeg path")
	}
	v := NewVideo("", "ffmpeg not found", &fakeHost{})
	if err := v.Acquire(); err == nil {
		t.Fatal("Acquire should fail without ffmpeg")
	}
	st := v.Status()
	if st.Available || st.Error == "" {
		t.Fatalf("status %+v", st)
	}
}

func TestVideoRidesTheOneSessionEndToEnd(t *testing.T) {
	ff := needFFmpeg(t)
	d := newFakeDevice(t, "pw")
	d.videoAU = accessUnits(genClip(t, ff, "320x240", 10, 2))
	v := NewVideo(ff, "", &fakeHost{})
	t.Cleanup(v.Close)
	if err := v.Acquire(); err != nil {
		t.Fatal(err)
	}
	cfg := d.dialConfig()
	cfg.Video = v
	ctx, cancel := context.WithTimeout(context.Background(), 20*time.Second)
	defer cancel()
	l, err := Dial(ctx, cfg)
	if err != nil {
		t.Fatalf("dial: %v", err)
	}
	t.Cleanup(l.Close)
	waitFor(t, func() bool { return v.Status().FramesDecoded >= 5 }, "frames over WebRTC")
	if f := v.Store().Latest(); f.W != 320 || f.H != 240 {
		t.Fatalf("frame %dx%d", f.W, f.H)
	}
	if d.plis.Load() == 0 {
		t.Fatal("no PLI reached the device")
	}
	d.mu.Lock()
	sdp := d.offers[0]
	nsess := len(d.sessions)
	d.mu.Unlock()
	if nsess != 1 || !strings.Contains(sdp, "m=video") || !strings.Contains(sdp, "H264") ||
		strings.Contains(sdp, "H265") || strings.Contains(sdp, "VP8") || strings.Contains(sdp, "AV1") {
		t.Fatalf("sessions=%d; the offer should carry one H.264-only video m-line:\n%s", nsess, sdp)
	}
	// The data channel still works on the same session.
	if got := rpc(t, l, "ping", nil); string(got) != `"pong"` {
		t.Fatalf("ping %s", got)
	}
}

func TestOfferHasNoVideoWithoutAVideoPipeline(t *testing.T) {
	d := newFakeDevice(t, "pw")
	dialFake(t, d)
	d.mu.Lock()
	defer d.mu.Unlock()
	if strings.Contains(d.offers[0], "m=video") {
		t.Fatal("a one-shot dial should not ask for video")
	}
}
