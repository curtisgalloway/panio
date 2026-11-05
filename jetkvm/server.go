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
	"fmt"
	"io"
	"net/http"
	"os"
	"strings"

	"github.com/coder/websocket"
)

// maxSendBytes bounds a POST /send body: one command line, sized so a
// full-length `type` is reachable (text plus the "type " verb).
const maxSendBytes int64 = maxTypeChars + int64(len("type "))

// newHandler is the daemon's HTTP API, behind the auth layer:
//
//	GET  /status   liveness and connection state
//	GET  /version  forwards `version`
//	POST /send     body = one command line; reply = OK data, or 503 + error
//	GET  /rfb      WebSocket: RFB 3.8 video (see README)
//	GET  /hid      WebSocket: command lines in, `evt ok|err ...` frames out
//	POST /stop     authenticated shutdown (calls stop)
func newHandler(owner *Owner, token string, stop func()) http.Handler {
	mux := http.NewServeMux()
	mux.HandleFunc("GET /status", func(w http.ResponseWriter, r *http.Request) {
		connected, kicked, lastErr := owner.Status()
		w.Header().Set("Content-Type", "application/json")
		_ = json.NewEncoder(w).Encode(map[string]any{
			"device":    owner.device,
			"pid":       os.Getpid(),
			"connected": connected,
			"kicked":    kicked,
			"last_err":  lastErr,
			"video":     owner.Video().Status(),
		})
	})
	mux.HandleFunc("GET /version", func(w http.ResponseWriter, r *http.Request) {
		data, err := owner.Send(r.Context(), "version")
		if err != nil {
			http.Error(w, err.Error(), http.StatusServiceUnavailable)
			return
		}
		_, _ = io.WriteString(w, data)
	})
	mux.HandleFunc("POST /send", func(w http.ResponseWriter, r *http.Request) {
		body, err := io.ReadAll(http.MaxBytesReader(w, r.Body, maxSendBytes))
		if err != nil {
			http.Error(w, "request body too large", http.StatusRequestEntityTooLarge)
			return
		}
		line := strings.TrimRight(string(body), "\r\n")
		// A blank body is a no-op, as in the /hid loop.
		if strings.TrimSpace(line) == "" {
			w.WriteHeader(http.StatusOK)
			return
		}
		data, err := owner.Send(r.Context(), line)
		if err != nil {
			http.Error(w, err.Error(), http.StatusServiceUnavailable)
			return
		}
		_, _ = io.WriteString(w, data)
	})
	mux.HandleFunc("POST /stop", func(w http.ResponseWriter, r *http.Request) {
		_, _ = io.WriteString(w, "hid daemon stopping\n")
		stop()
	})
	mux.HandleFunc("GET /rfb", func(w http.ResponseWriter, r *http.Request) {
		serveRFB(owner.Video(), w, r)
	})
	mux.HandleFunc("GET /hid", func(w http.ResponseWriter, r *http.Request) {
		serveHidWS(owner, w, r)
	})
	return requireAuth(token, mux)
}

// serveHidWS carries the HID serial protocol over a WebSocket. Every command,
// from any client or the CLI, broadcasts one `evt ok|err <line> :: <reply>`
// frame, so the issuer sees its own result there too and all clients observe
// one intermixed transcript.
func serveHidWS(owner *Owner, w http.ResponseWriter, r *http.Request) {
	// The auth layer has already checked Host, Origin and token; the library's
	// same-host Origin rule would reject the dashboard, which is served from a
	// different loopback port.
	c, err := websocket.Accept(w, r, &websocket.AcceptOptions{InsecureSkipVerify: true})
	if err != nil {
		return
	}
	defer c.CloseNow()
	c.SetReadLimit(maxSendBytes)
	ctx, cancel := context.WithCancel(r.Context())
	defer cancel()

	events, unsub := owner.Subscribe()
	defer unsub()
	go func() {
		defer cancel()
		for {
			select {
			case <-ctx.Done():
				return
			case ev := <-events:
				tag := "evt ok"
				if !ev.OK {
					tag = "evt err"
				}
				frame := fmt.Sprintf("%s %s :: %s", tag, ev.Line, ev.Reply)
				if c.Write(ctx, websocket.MessageText, []byte(frame)) != nil {
					return
				}
			}
		}
	}()
	for {
		typ, data, err := c.Read(ctx)
		if err != nil {
			return
		}
		if typ != websocket.MessageText {
			continue
		}
		line := strings.TrimRight(string(data), "\r\n")
		if strings.TrimSpace(line) == "" {
			continue
		}
		_, _ = owner.Send(ctx, line) // the result is broadcast as an evt frame
	}
}
