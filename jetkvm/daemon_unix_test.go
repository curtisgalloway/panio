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

//go:build !windows

package main

import (
	"bytes"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"testing"
)

func TestDiscoveryFileIsOwnerOnlyAndAtomic(t *testing.T) {
	dir := t.TempDir()
	p := filepath.Join(dir, "daemon.json")
	// A leftover temp file with loose permissions must not leak its mode.
	if err := os.WriteFile(p+".tmp", []byte("old"), 0o666); err != nil {
		t.Fatal(err)
	}
	if err := writePrivateFile(p, []byte(`{"pid":1}`)); err != nil {
		t.Fatal(err)
	}
	fi, err := os.Stat(p)
	if err != nil {
		t.Fatal(err)
	}
	if fi.Mode().Perm() != 0o600 {
		t.Fatalf("mode %v", fi.Mode().Perm())
	}
	if _, err := os.Stat(p + ".tmp"); !os.IsNotExist(err) {
		t.Fatal("the temp file must be renamed away")
	}
}

func TestRuntimeDirFallbackIsPrivateAndPerTarget(t *testing.T) {
	t.Setenv("PANIO_RUNTIME_DIR", "")
	base := t.TempDir()
	t.Setenv("PANIO_RUNTIME_BASE", base)
	dir, err := runtimeDir("target-machine", true)
	if err != nil {
		t.Fatal(err)
	}
	want := filepath.Join(base, "panio-"+strconv.Itoa(currentUID()), "hid", "target-machine")
	if dir != want {
		t.Fatalf("%s want %s", dir, want)
	}
	fi, _ := os.Stat(filepath.Dir(filepath.Dir(dir)))
	if fi.Mode().Perm() != 0o700 {
		t.Fatalf("base mode %v", fi.Mode().Perm())
	}
	if d, _ := runtimeDir("", false); filepath.Base(d) != "hid" {
		t.Fatalf("no target: %s", d)
	}
}

func TestEnsurePrivateDirTightensRefusesSymlink(t *testing.T) {
	root := t.TempDir()
	loose := filepath.Join(root, "loose")
	if err := os.Mkdir(loose, 0o755); err != nil {
		t.Fatal(err)
	}
	if err := ensurePrivateDir(loose); err != nil {
		t.Fatal(err)
	}
	if fi, _ := os.Stat(loose); fi.Mode().Perm() != 0o700 {
		t.Fatalf("not tightened: %v", fi.Mode().Perm())
	}
	link := filepath.Join(root, "link")
	if err := os.Symlink(loose, link); err != nil {
		t.Fatal(err)
	}
	if ensurePrivateDir(link) == nil {
		t.Fatal("a symlink must be refused")
	}
	file := filepath.Join(root, "file")
	_ = os.WriteFile(file, nil, 0o600)
	if ensurePrivateDir(file) == nil {
		t.Fatal("a regular file must be refused")
	}
}

// Reproduces the lock race from the Rust helpers: shutdown must leave the
// lock file in place and locked, or a second daemon locks a fresh inode while
// the first is still alive.
func TestShutdownCleanupLeavesLockFileLocked(t *testing.T) {
	dir := t.TempDir()
	lockPath := filepath.Join(dir, "daemon.lock")
	discPath := filepath.Join(dir, "daemon.json")
	first, err := acquireLock(lockPath)
	if err != nil {
		t.Fatal(err)
	}
	_ = os.WriteFile(discPath, []byte("{}"), 0o600)
	shutdownCleanup(discPath)
	if _, err := os.Stat(discPath); !os.IsNotExist(err) {
		t.Fatal("discovery file must be removed")
	}
	if _, err := os.Stat(lockPath); err != nil {
		t.Fatal("daemon.lock must stay on disk")
	}
	if second, err := acquireLock(lockPath); err == nil {
		second.Close()
		t.Fatal("a second daemon must not lock the path while the first runs")
	}
	first.Close()
	third, err := acquireLock(lockPath)
	if err != nil {
		t.Fatalf("the next daemon can lock once the first is gone: %v", err)
	}
	third.Close()
}

// `jetkvm stop --target T` (flag after the verb) must find T's daemon. It used
// to drop the flag and report "no hid daemon running" for a live daemon. A
// token-less record makes stop fail at the request, which proves it was found.
func TestStopTargetAfterTheVerbFindsTheDaemon(t *testing.T) {
	t.Setenv("PANIO_RUNTIME_DIR", "")
	t.Setenv("PANIO_RUNTIME_BASE", t.TempDir())
	dir, err := runtimeDir("target-machine", true)
	if err != nil {
		t.Fatal(err)
	}
	_ = os.WriteFile(filepath.Join(dir, "daemon.json"),
		[]byte(`{"pid":`+strconv.Itoa(os.Getpid())+`,"port":7}`), 0o600)
	for _, args := range [][]string{
		{"stop", "--target", "target-machine"},
		{"--target", "target-machine", "stop"},
	} {
		var out, errb bytes.Buffer
		code := run(args, strings.NewReader(""), &out, &errb)
		if code != exitFailure || !strings.Contains(errb.String(), "no token") {
			t.Fatalf("%v: code %d stdout %q stderr %q", args, code, out.String(), errb.String())
		}
	}
	var out, errb bytes.Buffer
	if code := run([]string{"stop", "extra"}, strings.NewReader(""), &out, &errb); code != exitUsage {
		t.Fatalf("stray argument: code %d", code)
	}
}

func TestDiscoverIgnoresDeadPidAndGarbage(t *testing.T) {
	dir := t.TempDir()
	t.Setenv("PANIO_RUNTIME_DIR", dir)
	if discover("") != nil {
		t.Fatal("no file, no daemon")
	}
	_ = os.WriteFile(filepath.Join(dir, "daemon.json"), []byte("garbage"), 0o600)
	if discover("") != nil {
		t.Fatal("garbage is not a daemon")
	}
	_ = os.WriteFile(filepath.Join(dir, "daemon.json"),
		[]byte(`{"pid":`+strconv.Itoa(os.Getpid())+`,"port":7,"token":"t","device":"d"}`), 0o600)
	d := discover("")
	if d == nil || d.Port != 7 || d.Token != "t" || d.Device != "d" {
		t.Fatalf("%+v", d)
	}
	_ = os.WriteFile(filepath.Join(dir, "daemon.json"), []byte(`{"pid":0,"port":7}`), 0o600)
	if discover("") != nil {
		t.Fatal("pid 0 must never read as alive")
	}
}
