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
	"crypto/rand"
	"crypto/subtle"
	"encoding/hex"
	"fmt"
	"log/slog"
	"net/http"
	"strings"
)

// Request authentication for the daemon's localhost HTTP API (the same rules
// as the Rust helpers' auth.rs).
//
// Binding 127.0.0.1 keeps other machines out, not other origins: any web page
// in the operator's browser can POST to a loopback port or open a WebSocket
// to it, and a DNS-rebinding page can do so under a name that looks like its
// own. Every request therefore passes three checks, in this order:
//
//  1. Host must name a loopback address (defeats DNS rebinding).
//  2. Origin, when the browser sends one, must be a loopback origin too.
//  3. Token: Authorization: Bearer <token>, or a token=<token> query
//     parameter (WebSockets and image loads cannot set headers). The token is
//     published in daemon.json, written owner-only.
//
// Access-Control-Allow-Origin is never "*": an allowed Origin is echoed back,
// with Vary: Origin, so the dashboard's cross-port requests still work.

var loopbackHosts = []string{"127.0.0.1", "localhost", "[::1]"}

const tokenBytes = 32

// generateToken returns a fresh hex-encoded token from the OS entropy source.
func generateToken() (string, error) {
	b := make([]byte, tokenBytes)
	if _, err := rand.Read(b); err != nil {
		return "", fmt.Errorf("generating the daemon token: %w", err)
	}
	return hex.EncodeToString(b), nil
}

// hostPart is the host part of a host[:port] authority; a bracketed IPv6
// literal keeps its brackets.
func hostPart(authority string) string {
	if strings.HasPrefix(authority, "[") {
		if i := strings.Index(authority, "]"); i >= 0 {
			return authority[:i+1]
		}
		return authority
	}
	if i := strings.LastIndex(authority, ":"); i >= 0 {
		return authority[:i]
	}
	return authority
}

func isLoopbackAuthority(authority string) bool {
	h := strings.ToLower(hostPart(strings.TrimSpace(authority)))
	for _, l := range loopbackHosts {
		if h == l {
			return true
		}
	}
	return false
}

// isLoopbackOrigin accepts http(s)://<loopback>[:port] and nothing else;
// "null" and every other scheme are rejected.
func isLoopbackOrigin(origin string) bool {
	rest, ok := strings.CutPrefix(origin, "http://")
	if !ok {
		rest, ok = strings.CutPrefix(origin, "https://")
	}
	if !ok || rest == "" || strings.Contains(rest, "/") {
		return false
	}
	return isLoopbackAuthority(rest)
}

func bearer(r *http.Request) (string, bool) {
	v := r.Header.Get("Authorization")
	scheme, rest, ok := strings.Cut(strings.TrimSpace(v), " ")
	if !ok || !strings.EqualFold(scheme, "bearer") {
		return "", false
	}
	return strings.TrimSpace(rest), true
}

func queryToken(rawQuery string) (string, bool) {
	for _, pair := range strings.Split(rawQuery, "&") {
		if t, ok := strings.CutPrefix(pair, "token="); ok {
			return t, true
		}
	}
	return "", false
}

func tokenEq(a, b string) bool {
	return subtle.ConstantTimeCompare([]byte(a), []byte(b)) == 1
}

func presentsToken(r *http.Request, token string) bool {
	hdr := false
	if t, ok := bearer(r); ok {
		hdr = tokenEq(t, token)
	}
	q := false
	if t, ok := queryToken(r.URL.RawQuery); ok {
		q = tokenEq(t, token)
	}
	return hdr || q
}

// requireAuth wraps h with the three checks above.
func requireAuth(token string, h http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if !isLoopbackAuthority(r.Host) || r.Host == "" {
			slog.Debug("rejected request: Host is missing or not loopback")
			http.Error(w, "forbidden: this daemon answers only loopback Host names", http.StatusForbidden)
			return
		}
		origin := r.Header.Get("Origin")
		_, hasOrigin := r.Header["Origin"]
		if hasOrigin && !isLoopbackOrigin(origin) {
			slog.Debug("rejected request: Origin is not a loopback origin")
			http.Error(w, "forbidden: cross-origin requests are not accepted", http.StatusForbidden)
			return
		}
		cors := func() {
			if hasOrigin {
				w.Header().Set("Access-Control-Allow-Origin", origin)
				w.Header().Add("Vary", "Origin")
			}
		}
		if !presentsToken(r, token) {
			slog.Debug("rejected request: missing or wrong token", "path", r.URL.Path)
			cors()
			w.Header().Set("WWW-Authenticate", "Bearer")
			http.Error(w, "unauthorized: send the token from daemon.json as "+
				"`Authorization: Bearer <token>` or `?token=<token>`", http.StatusUnauthorized)
			return
		}
		cors()
		h.ServeHTTP(w, r)
	})
}
