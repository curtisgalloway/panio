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
	"io"
	"net/http"
	"net/http/httptest"
	"strings"
	"sync"
	"sync/atomic"
	"testing"
	"time"
)

// fakeLink is an rpcLink that records calls and can be ended on demand.
type fakeLink struct {
	mu     sync.Mutex
	calls  []string
	done   chan struct{}
	once   sync.Once
	kicked bool
	delay  time.Duration
}

func newFakeLink() *fakeLink { return &fakeLink{done: make(chan struct{})} }

func (f *fakeLink) Call(ctx context.Context, method string, params any) (json.RawMessage, error) {
	if f.delay > 0 {
		time.Sleep(f.delay)
	}
	b, _ := json.Marshal(params)
	f.mu.Lock()
	f.calls = append(f.calls, method+" "+string(b))
	f.mu.Unlock()
	if method == "ping" {
		return json.RawMessage(`"pong"`), nil
	}
	return json.RawMessage("null"), nil
}
func (f *fakeLink) Done() <-chan struct{} { return f.done }
func (f *fakeLink) Kicked() bool          { return f.kicked }
func (f *fakeLink) Close()                { f.once.Do(func() { close(f.done) }) }
func (f *fakeLink) recorded() []string {
	f.mu.Lock()
	defer f.mu.Unlock()
	return append([]string(nil), f.calls...)
}

func testOwner(t *testing.T, dial Dialer) *Owner {
	t.Helper()
	o := NewOwner("target.example", dial)
	o.session.t = timing{}
	ctx, cancel := context.WithCancel(context.Background())
	t.Cleanup(cancel)
	go o.worker(ctx)
	return o
}

func TestOwnerConnectsLazilyAndReusesTheSession(t *testing.T) {
	var dials atomic.Int32
	link := newFakeLink()
	o := testOwner(t, func(context.Context) (rpcLink, error) { dials.Add(1); return link, nil })
	if _, err := o.Send(context.Background(), "version"); err != nil || dials.Load() != 0 {
		t.Fatalf("version must not connect: %v dials=%d", err, dials.Load())
	}
	for i := 0; i < 3; i++ {
		if _, err := o.Send(context.Background(), "ping"); err != nil {
			t.Fatal(err)
		}
	}
	if dials.Load() != 1 {
		t.Fatalf("dials=%d, want one session", dials.Load())
	}
}

func TestOwnerSerializesCommandsInArrivalOrder(t *testing.T) {
	link := newFakeLink()
	o := testOwner(t, func(context.Context) (rpcLink, error) { return link, nil })
	var wg sync.WaitGroup
	for i := 0; i < 20; i++ {
		wg.Add(1)
		go func() {
			defer wg.Done()
			_, _ = o.Send(context.Background(), "key A")
		}()
	}
	wg.Wait()
	calls := link.recorded()
	// Each tap is a press report then a release report; interleaving between
	// commands would break the strict alternation.
	for i, c := range calls {
		want := `keyboardReport {"keys":[4],"modifier":0}`
		if i%2 == 1 {
			want = `keyboardReport {"keys":[],"modifier":0}`
		}
		if c != want {
			t.Fatalf("call %d = %s, want %s (commands interleaved)", i, c, want)
		}
	}
	if len(calls) != 40 {
		t.Fatalf("%d calls", len(calls))
	}
}

func TestOwnerRefusesEmbeddedNewlineBeforeQueueing(t *testing.T) {
	link := newFakeLink()
	o := testOwner(t, func(context.Context) (rpcLink, error) { return link, nil })
	if _, err := o.Send(context.Background(), "type a\nkey ENTER"); err == nil {
		t.Fatal("expected a refusal")
	}
	if len(link.recorded()) != 0 {
		t.Fatal("nothing may reach the device")
	}
}

func TestOwnerDropsACommandWhoseClientGaveUp(t *testing.T) {
	link := newFakeLink()
	o := NewOwner("x", func(context.Context) (rpcLink, error) { return link, nil })
	o.session.t = timing{}
	ctx, cancel := context.WithCancel(context.Background())
	cancel()
	o.handle(request{ctx: ctx, line: "key A", reply: make(chan result, 1)})
	if len(link.recorded()) != 0 {
		t.Fatalf("a cancelled command was injected late: %v", link.recorded())
	}
}

func TestOwnerDialFailureSurfacesAndRecovers(t *testing.T) {
	var fail atomic.Bool
	fail.Store(true)
	link := newFakeLink()
	o := testOwner(t, func(context.Context) (rpcLink, error) {
		if fail.Load() {
			return nil, errors.New("connection refused")
		}
		return link, nil
	})
	if _, err := o.Send(context.Background(), "ping"); err == nil || !strings.Contains(err.Error(), "connection refused") {
		t.Fatalf("got %v", err)
	}
	if c, _, last := o.Status(); c || !strings.Contains(last, "connection refused") {
		t.Fatalf("status: %v %q", c, last)
	}
	fail.Store(false)
	if _, err := o.Send(context.Background(), "ping"); err != nil {
		t.Fatalf("an explicit command must retry the connection: %v", err)
	}
}

// The browser-takeover policy: after otherSessionConnected the daemon must not
// redial on its own (that would fight the person for the one session), but the
// next explicit command reconnects.
func TestOwnerDoesNotRedialAfterBeingKickedButCommandsReconnect(t *testing.T) {
	var dials atomic.Int32
	var current atomic.Pointer[fakeLink]
	o := testOwner(t, func(context.Context) (rpcLink, error) {
		dials.Add(1)
		l := newFakeLink()
		current.Store(l)
		return l, nil
	})
	if _, err := o.Send(context.Background(), "ping"); err != nil {
		t.Fatal(err)
	}
	first := current.Load()
	first.kicked = true
	first.Close()
	deadline := time.Now().Add(2 * time.Second)
	for {
		if _, kicked, _ := o.Status(); kicked {
			break
		}
		if time.Now().After(deadline) {
			t.Fatal("the owner never noticed the takeover")
		}
		time.Sleep(5 * time.Millisecond)
	}
	// Run the maintainer for a while: it must stay idle.
	ctx, cancel := context.WithCancel(context.Background())
	go o.maintain(ctx)
	time.Sleep(1500 * time.Millisecond)
	cancel()
	if dials.Load() != 1 {
		t.Fatalf("the maintainer redialed after a takeover (dials=%d)", dials.Load())
	}
	if _, err := o.Send(context.Background(), "ping"); err != nil {
		t.Fatal(err)
	}
	if dials.Load() != 2 {
		t.Fatalf("an explicit command must reconnect (dials=%d)", dials.Load())
	}
	if _, kicked, _ := o.Status(); kicked {
		t.Fatal("kicked state must clear once reconnected")
	}
}

func TestOwnerBackgroundReconnectsAfterAPlainDrop(t *testing.T) {
	var dials atomic.Int32
	var current atomic.Pointer[fakeLink]
	o := NewOwner("x", func(context.Context) (rpcLink, error) {
		dials.Add(1)
		l := newFakeLink()
		current.Store(l)
		return l, nil
	})
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	go o.maintain(ctx)
	waitFor(t, func() bool { return o.Connected() }, "initial background connect")
	current.Load().Close() // a network drop, not a takeover
	waitFor(t, func() bool { return dials.Load() >= 2 && o.Connected() }, "background reconnect")
}

func waitFor(t *testing.T, cond func() bool, what string) {
	t.Helper()
	deadline := time.Now().Add(8 * time.Second)
	for !cond() {
		if time.Now().After(deadline) {
			t.Fatalf("timed out: %s", what)
		}
		time.Sleep(10 * time.Millisecond)
	}
}

func TestOwnerReleaseForShutdownOnlyWhenConnected(t *testing.T) {
	link := newFakeLink()
	o := testOwner(t, func(context.Context) (rpcLink, error) { return link, nil })
	if err := o.ReleaseForShutdown(time.Second); err != nil || len(link.recorded()) != 0 {
		t.Fatalf("no session: must not dial just to release (%v, %v)", err, link.recorded())
	}
	_, _ = o.Send(context.Background(), "down A")
	_, _ = o.Send(context.Background(), "mdown left")
	if err := o.ReleaseForShutdown(time.Second); err != nil {
		t.Fatal(err)
	}
	c := link.recorded()
	if n := len(c); c[n-2] != `keyboardReport {"keys":[],"modifier":0}` || c[n-1] != `relMouseReport {"buttons":0,"dx":0,"dy":0}` {
		t.Fatalf("tail %v", c[n-2:])
	}
}

func TestOwnerBroadcastsEvents(t *testing.T) {
	link := newFakeLink()
	o := testOwner(t, func(context.Context) (rpcLink, error) { return link, nil })
	events, unsub := o.Subscribe()
	defer unsub()
	_, _ = o.Send(context.Background(), "ping")
	_, _ = o.Send(context.Background(), "version")
	_, _ = o.Send(context.Background(), "key NOPE")
	want := []Event{
		{"ping", true, "OK"},
		{"version", true, "OK " + versionReply},
		{"key NOPE", false, "unknown key name: NOPE"},
	}
	for _, w := range want {
		select {
		case ev := <-events:
			if ev != w {
				t.Fatalf("event %+v want %+v", ev, w)
			}
		case <-time.After(time.Second):
			t.Fatalf("no event for %q", w.Line)
		}
	}
}

func serverFor(t *testing.T, stop func()) (*httptest.Server, *Owner, *fakeLink) {
	t.Helper()
	link := newFakeLink()
	o := testOwner(t, func(context.Context) (rpcLink, error) { return link, nil })
	srv := httptest.NewServer(newHandler(o, testToken, stop))
	t.Cleanup(srv.Close)
	return srv, o, link
}

func post(t *testing.T, url, token, body string) (int, string) {
	t.Helper()
	req, _ := http.NewRequest(http.MethodPost, url, strings.NewReader(body))
	if token != "" {
		req.Header.Set("Authorization", "Bearer "+token)
	}
	resp, err := http.DefaultClient.Do(req)
	if err != nil {
		t.Fatal(err)
	}
	defer resp.Body.Close()
	b, _ := io.ReadAll(resp.Body)
	return resp.StatusCode, string(b)
}

func TestServerStopRequiresTheToken(t *testing.T) {
	var stopped atomic.Int32
	srv, _, _ := serverFor(t, func() { stopped.Add(1) })
	for _, tok := range []string{"", "wrong"} {
		if code, _ := post(t, srv.URL+"/stop", tok, ""); code != 401 {
			t.Fatalf("token %q: %d", tok, code)
		}
	}
	if stopped.Load() != 0 {
		t.Fatal("an unauthenticated /stop woke the shutdown")
	}
	if code, _ := post(t, srv.URL+"/stop", testToken, ""); code != 200 || stopped.Load() != 1 {
		t.Fatalf("valid token: code %d stopped %d", code, stopped.Load())
	}
}

func TestServerSendReturnsDataOr503(t *testing.T) {
	srv, _, link := serverFor(t, func() {})
	if code, body := post(t, srv.URL+"/send", testToken, "version"); code != 200 || body != versionReply {
		t.Fatalf("version: %d %q", code, body)
	}
	if code, body := post(t, srv.URL+"/send", testToken, "key NOPE"); code != 503 || !strings.Contains(body, "unknown key name") {
		t.Fatalf("bad key: %d %q", code, body)
	}
	if code, _ := post(t, srv.URL+"/send", testToken, "ping"); code != 200 {
		t.Fatalf("ping: %d", code)
	}
	if len(link.recorded()) != 1 {
		t.Fatalf("calls %v", link.recorded())
	}
}

func TestServerBlankSendIsANoOpAndOversizeIsRefused(t *testing.T) {
	srv, _, link := serverFor(t, func() {})
	if code, _ := post(t, srv.URL+"/send", testToken, ""); code != 200 {
		t.Fatalf("blank: %d", code)
	}
	if code, _ := post(t, srv.URL+"/send", testToken, "\r\n"); code != 200 {
		t.Fatalf("newline only: %d", code)
	}
	if len(link.recorded()) != 0 {
		t.Fatal("a blank body must not be forwarded as a command")
	}
	if code, _ := post(t, srv.URL+"/send", testToken, "type "+strings.Repeat("a", maxTypeChars)); code != 200 {
		t.Fatalf("a full-length type line must fit: %d", code)
	}
	if code, _ := post(t, srv.URL+"/send", testToken, "type "+strings.Repeat("a", maxTypeChars+10)); code != 413 {
		t.Fatalf("oversize: %d", code)
	}
}

func TestServerStatusAndVersion(t *testing.T) {
	srv, _, _ := serverFor(t, func() {})
	get := func(path string) (int, string) {
		req, _ := http.NewRequest(http.MethodGet, srv.URL+path, nil)
		req.Header.Set("Authorization", "Bearer "+testToken)
		resp, err := http.DefaultClient.Do(req)
		if err != nil {
			t.Fatal(err)
		}
		defer resp.Body.Close()
		b, _ := io.ReadAll(resp.Body)
		return resp.StatusCode, string(b)
	}
	code, body := get("/status")
	var st map[string]any
	if code != 200 || json.Unmarshal([]byte(body), &st) != nil || st["device"] != "target.example" || st["pid"] == nil {
		t.Fatalf("status %d %s", code, body)
	}
	if code, body := get("/version"); code != 200 || body != versionReply {
		t.Fatalf("version %d %q", code, body)
	}
}

func TestCLIWithoutPasswordExitsThree(t *testing.T) {
	t.Setenv(passwordEnv, "")
	t.Setenv("PANIO_RUNTIME_DIR", t.TempDir())
	var out, errb strings.Builder
	code := run([]string{"-d", "192.0.2.10", "ping"}, strings.NewReader(""), &out, &errb)
	if code != exitNotConfigured || !strings.Contains(errb.String(), "JETKVM_PASSWORD") {
		t.Fatalf("exit %d stderr %q", code, errb.String())
	}
}

func TestCLIUsageErrorsExitTwo(t *testing.T) {
	t.Setenv("PANIO_RUNTIME_DIR", t.TempDir())
	for _, args := range [][]string{
		{},
		{"ping"}, // no -d
		{"-d", "192.0.2.10", "frobnicate"},
		{"-d", "http://192.0.2.10/", "ping"},
		{"-d", "192.0.2.10", "move", "1"},
		{"-d", "192.0.2.10", "type"},
		{"-bogus-flag"},
	} {
		var out, errb strings.Builder
		if code := run(args, strings.NewReader(""), &out, &errb); code != exitUsage {
			t.Errorf("%v: exit %d (stderr %q)", args, code, errb.String())
		}
	}
}

func TestCLIVersionNeedsNoDeviceOrPassword(t *testing.T) {
	var out, errb strings.Builder
	if code := run([]string{"version"}, strings.NewReader(""), &out, &errb); code != 0 || strings.TrimSpace(out.String()) != versionReply {
		t.Fatalf("%d %q %q", code, out.String(), errb.String())
	}
}

func TestSplitHost(t *testing.T) {
	good := []string{"192.0.2.10", "192.0.2.10:8080", "jetkvm.example", "jetkvm.example:80", "[::1]", "[::1]:80"}
	bad := []string{"", "http://x", "x/y", "user@x", "x:", ":80", "x:99999", "x:abc", "::1", "x y"}
	for _, g := range good {
		if _, err := splitHost(g); err != nil {
			t.Errorf("%q: %v", g, err)
		}
	}
	for _, b := range bad {
		if _, err := splitHost(b); err == nil {
			t.Errorf("%q must be refused", b)
		}
	}
}

func TestDirectOneShotAndRunFile(t *testing.T) {
	d := newFakeDevice(t, "pw")
	t.Setenv("PANIO_RUNTIME_DIR", t.TempDir()) // no daemon here
	t.Setenv(passwordEnv, "")
	pwFile := writeSecretFile(t, "pw\n", 0o600)
	seq := writeSecretFile(t, "# comment\ntype x\ndelay 1\nclick right\n", 0o600)
	var out, errb strings.Builder
	base := []string{"-d", d.host(), "--password-file", pwFile}
	defaultSettingEngine = loopbackEngine()
	t.Cleanup(func() { defaultSettingEngine = nil })
	if code := run(append(base, "ping"), strings.NewReader(""), &out, &errb); code != 0 || strings.TrimSpace(out.String()) != "OK" {
		t.Fatalf("ping: %d %q %q", code, out.String(), errb.String())
	}
	out.Reset()
	if code := run(append(base, "run", seq), strings.NewReader(""), &out, &errb); code != 0 || strings.TrimSpace(out.String()) != "OK (2 commands)" {
		t.Fatalf("run: %d %q %q", code, out.String(), errb.String())
	}
	calls := d.recorded()
	if len(calls) != 1+2+2 || calls[len(calls)-2] != `relMouseReport {"buttons":2,"dx":0,"dy":0}` {
		t.Fatalf("calls %v", calls)
	}
	// Wrong password: exit 3.
	out.Reset()
	t.Setenv(passwordEnv, "bad")
	if code := run([]string{"-d", d.host(), "ping"}, strings.NewReader(""), &out, &errb); code != exitNotConfigured {
		t.Fatalf("bad password exit %d", code)
	}
}
