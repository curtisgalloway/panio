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
	"encoding/json"
	"errors"
	"fmt"
	"log/slog"
	"net"
	"net/http"
	"os"
	"os/signal"
	"path/filepath"
	"runtime"
	"syscall"
	"time"
)

// discoveryName is the panio channel name, not the binary name, so panio
// finds the daemon without knowing which helper implements the channel.
const discoveryName = "hid"

// releaseTimeout bounds the release of held keys and buttons at shutdown.
const releaseTimeout = 2 * time.Second

// Discovery is the daemon.json record panio reads.
type Discovery struct {
	PID    int    `json:"pid"`
	Port   int    `json:"port"`
	Token  string `json:"token,omitempty"`
	Device string `json:"device"`
}

// runtimeDir is the daemon's runtime directory. panio passes the canonical
// location as PANIO_RUNTIME_DIR; the fallback is
// $PANIO_RUNTIME_BASE|/tmp/panio-<uid>/hid[/<target>] for standalone use.
// With create false nothing is made, so a one-shot only reads.
func runtimeDir(target string, create bool) (string, error) {
	if d := os.Getenv("PANIO_RUNTIME_DIR"); d != "" {
		if create {
			if err := os.MkdirAll(d, 0o700); err != nil {
				return "", err
			}
		}
		return d, nil
	}
	root := os.Getenv("PANIO_RUNTIME_BASE")
	if root == "" {
		root = "/tmp"
		if runtime.GOOS == "windows" {
			root = os.TempDir()
		}
	}
	base := filepath.Join(root, fmt.Sprintf("panio-%d", currentUID()))
	dir := filepath.Join(base, discoveryName)
	if target != "" {
		dir = filepath.Join(dir, target)
	}
	if create {
		if err := ensurePrivateDir(base); err != nil {
			return "", err
		}
		if err := os.MkdirAll(dir, 0o700); err != nil {
			return "", err
		}
	}
	return dir, nil
}

// writePrivateFile writes path atomically (temp file, then rename), readable
// by the owner only.
func writePrivateFile(path string, data []byte) error {
	tmp := path + ".tmp"
	_ = os.Remove(tmp) // the mode applies only on create; never reuse a leftover
	f, err := os.OpenFile(tmp, os.O_WRONLY|os.O_CREATE|os.O_EXCL, 0o600)
	if err != nil {
		return err
	}
	if _, err := f.Write(data); err != nil {
		f.Close()
		return err
	}
	if err := f.Close(); err != nil {
		return err
	}
	return os.Rename(tmp, path)
}

// discover reads the discovery file, or returns nil when no live daemon is
// recorded.
func discover(target string) *Discovery {
	dir, err := runtimeDir(target, false)
	if err != nil {
		return nil
	}
	b, err := os.ReadFile(filepath.Join(dir, "daemon.json"))
	if err != nil {
		return nil
	}
	var d Discovery
	if json.Unmarshal(b, &d) != nil || !pidAlive(d.PID) {
		return nil
	}
	return &d
}

// acquireLock takes the daemon's advisory lock at path, creating the file.
// The file is never unlinked on shutdown: unlinking while the lock is held
// would let a next daemon lock a fresh inode while this process, holding the
// old one, is still alive.
func acquireLock(path string) (*os.File, error) {
	f, err := os.OpenFile(path, os.O_CREATE|os.O_WRONLY, 0o600)
	if err != nil {
		return nil, err
	}
	if err := lockFile(f); err != nil {
		f.Close()
		return nil, errors.New("another hid daemon is already running")
	}
	return f, nil
}

// shutdownCleanup removes only the discovery file; see acquireLock.
func shutdownCleanup(discoveryPath string) { _ = os.Remove(discoveryPath) }

// serveDaemon runs `jetkvm serve`: lock, listen, publish discovery, serve
// until stopped. It returns after graceful shutdown.
func serveDaemon(device, target string, port int, owner *Owner) error {
	dir, err := runtimeDir(target, true)
	if err != nil {
		return err
	}
	lock, err := acquireLock(filepath.Join(dir, "daemon.lock"))
	if err != nil {
		return err
	}
	defer lock.Close()

	ln, err := net.Listen("tcp", fmt.Sprintf("127.0.0.1:%d", port))
	if err != nil {
		return err
	}
	token, err := generateToken()
	if err != nil {
		return err
	}
	discPath := filepath.Join(dir, "daemon.json")
	rec, _ := json.Marshal(Discovery{
		PID: os.Getpid(), Port: ln.Addr().(*net.TCPAddr).Port, Token: token, Device: device,
	})
	if err := writePrivateFile(discPath, rec); err != nil {
		return fmt.Errorf("writing discovery file: %w", err)
	}
	slog.Info("jetkvm hid daemon listening", "addr", ln.Addr().String(), "device", device)

	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	owner.Start(ctx)

	stop := make(chan struct{}, 1)
	srv := &http.Server{Handler: newHandler(owner, token, func() {
		select {
		case stop <- struct{}{}:
		default:
		}
	})}
	errc := make(chan error, 1)
	go func() { errc <- srv.Serve(ln) }()

	sig := make(chan os.Signal, 1)
	signal.Notify(sig, os.Interrupt, syscall.SIGTERM)
	select {
	case <-sig:
		slog.Info("shutdown signal received")
	case <-stop:
		slog.Info("stop requested over HTTP")
	case err := <-errc:
		shutdownCleanup(discPath)
		return err
	}
	// This daemon is the only thing that remembers what it pressed; leave the
	// target with nothing held.
	if err := owner.ReleaseForShutdown(releaseTimeout); err != nil {
		slog.Warn("shutdown", "err", err)
	}
	shutdownCleanup(discPath)
	sctx, scancel := context.WithTimeout(context.Background(), 300*time.Millisecond)
	defer scancel()
	_ = srv.Shutdown(sctx)
	_ = srv.Close() // the /hid WebSocket is long-lived
	cancel()
	if v := owner.Video(); v != nil {
		v.Close() // kills ffmpeg
	}
	slog.Info("jetkvm hid daemon shut down")
	return nil
}
