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
	"net/http"
	"net/http/httptest"
	"testing"
)

const testToken = "3f1c9b0e7a5d2c4f8e6b1a0d9c7e5f3a2b4c6d8e0f1a3b5c7d9e1f2a4b6c8d0e"

func authedApp() http.Handler {
	return requireAuth(testToken, http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		_, _ = w.Write([]byte("secret"))
	}))
}

func do(h http.Handler, host string, hdr map[string]string, target string) *httptest.ResponseRecorder {
	req := httptest.NewRequest(http.MethodGet, target, nil)
	req.Host = host
	for k, v := range hdr {
		req.Header.Set(k, v)
	}
	rec := httptest.NewRecorder()
	h.ServeHTTP(rec, req)
	return rec
}

func bearerHdr() map[string]string {
	return map[string]string{"Authorization": "Bearer " + testToken}
}

func TestAuthAcceptsBearerAndQueryToken(t *testing.T) {
	if rec := do(authedApp(), "127.0.0.1:9", bearerHdr(), "/x"); rec.Code != 200 {
		t.Fatalf("bearer: %d", rec.Code)
	}
	if rec := do(authedApp(), "localhost:9", nil, "/x?token="+testToken); rec.Code != 200 {
		t.Fatalf("query: %d", rec.Code)
	}
	if rec := do(authedApp(), "[::1]:9", map[string]string{"Authorization": "bearer " + testToken}, "/x"); rec.Code != 200 {
		t.Fatalf("ipv6 + lowercase scheme: %d", rec.Code)
	}
}

func TestAuthRejectsMissingOrWrongToken(t *testing.T) {
	for name, hdr := range map[string]map[string]string{
		"none":   nil,
		"wrong":  {"Authorization": "Bearer nope"},
		"scheme": {"Authorization": "Basic " + testToken},
		"prefix": {"Authorization": "Bearer " + testToken[:10]},
	} {
		rec := do(authedApp(), "127.0.0.1:9", hdr, "/x")
		if rec.Code != 401 {
			t.Errorf("%s: %d", name, rec.Code)
		}
		if rec.Header().Get("WWW-Authenticate") != "Bearer" {
			t.Errorf("%s: no WWW-Authenticate", name)
		}
	}
	if rec := do(authedApp(), "127.0.0.1:9", nil, "/x?token=wrong"); rec.Code != 401 {
		t.Errorf("wrong query token: %d", rec.Code)
	}
}

// A DNS-rebinding page reaches the daemon under its own name; the token alone
// must not be enough.
func TestAuthRejectsNonLoopbackHost(t *testing.T) {
	for _, host := range []string{"evil.example", "evil.example:80", "10.0.0.1:9", "127.0.0.1.evil.example", ""} {
		if rec := do(authedApp(), host, bearerHdr(), "/x"); rec.Code != 403 {
			t.Errorf("Host %q: %d", host, rec.Code)
		}
	}
}

func TestAuthOriginRules(t *testing.T) {
	for _, origin := range []string{"https://evil.example", "null", "file://", "http://127.0.0.1.evil.example", "http://localhost/x", "ftp://localhost"} {
		hdr := bearerHdr()
		hdr["Origin"] = origin
		rec := do(authedApp(), "127.0.0.1:9", hdr, "/x")
		if rec.Code != 403 {
			t.Errorf("Origin %q: %d", origin, rec.Code)
		}
		if rec.Header().Get("Access-Control-Allow-Origin") != "" {
			t.Errorf("Origin %q: CORS header on a forbidden request", origin)
		}
	}
}

func TestAuthEchoesLoopbackOriginNeverStar(t *testing.T) {
	for _, origin := range []string{"http://127.0.0.1:8080", "http://localhost:3000", "https://[::1]:1"} {
		hdr := bearerHdr()
		hdr["Origin"] = origin
		rec := do(authedApp(), "127.0.0.1:9", hdr, "/x")
		if rec.Code != 200 || rec.Header().Get("Access-Control-Allow-Origin") != origin {
			t.Errorf("Origin %q: %d %q", origin, rec.Code, rec.Header().Get("Access-Control-Allow-Origin"))
		}
		if rec.Header().Get("Vary") != "Origin" {
			t.Errorf("Origin %q: Vary %q", origin, rec.Header().Get("Vary"))
		}
	}
	// A rejected token still gets the echo so the dashboard can read the 401.
	hdr := map[string]string{"Origin": "http://localhost:3000"}
	rec := do(authedApp(), "127.0.0.1:9", hdr, "/x")
	if rec.Code != 401 || rec.Header().Get("Access-Control-Allow-Origin") != "http://localhost:3000" {
		t.Errorf("401 echo: %d %q", rec.Code, rec.Header().Get("Access-Control-Allow-Origin"))
	}
}

func TestGenerateTokenIsRandomHex(t *testing.T) {
	a, err := generateToken()
	b, _ := generateToken()
	if err != nil || len(a) != 64 || a == b {
		t.Fatalf("%q %q %v", a, b, err)
	}
}

func TestHostPart(t *testing.T) {
	for in, want := range map[string]string{
		"127.0.0.1:80": "127.0.0.1", "localhost": "localhost", "[::1]:80": "[::1]", "[::1]": "[::1]",
	} {
		if got := hostPart(in); got != want {
			t.Errorf("%q: %q want %q", in, got, want)
		}
	}
}
