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

//go:build !windows

package main

import (
	"syscall"
	"testing"
	"time"
)

func TestCloseKillsFFmpegWithNoOrphan(t *testing.T) {
	ff := needFFmpeg(t)
	v := NewVideo(ff, "", &fakeHost{})
	if err := v.Acquire(); err != nil {
		t.Fatal(err)
	}
	feed(v, packetize(accessUnits(genClip(t, ff, "320x240", 10, 1))))
	waitFrames(t, v, 2)
	v.mu.Lock()
	pid := v.dec.pid
	v.mu.Unlock()
	if syscall.Kill(pid, 0) != nil {
		t.Fatal("ffmpeg is not running before Close")
	}
	v.Close()
	deadline := time.Now().Add(3 * time.Second)
	for time.Now().Before(deadline) {
		if syscall.Kill(pid, 0) != nil {
			return
		}
		time.Sleep(20 * time.Millisecond)
	}
	t.Fatalf("ffmpeg pid %d survived Close", pid)
}
