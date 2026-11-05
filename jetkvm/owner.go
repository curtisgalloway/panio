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
	"errors"
	"fmt"
	"log/slog"
	"strings"
	"sync"
	"time"
)

const (
	// rpcTimeout bounds one JSON-RPC round trip.
	rpcTimeout = 5 * time.Second
	// sendTimeout bounds one client's wait for a reply. The worker handles one
	// command at a time, so a command that never finishes would otherwise
	// hold every client queued behind it. A full-length `type` takes a while,
	// hence the generous ceiling.
	sendTimeout = 120 * time.Second
	// dialTimeout bounds one connection attempt.
	dialTimeout = 25 * time.Second
	// pingEvery is how often an idle connection is checked.
	pingEvery = 20 * time.Second
	// maxBackoff caps the delay between background reconnect attempts.
	maxBackoff = 30 * time.Second
	reqCap     = 256
	eventCap   = 256
)

// rpcLink is what the owner needs from a live session (*Link in production, a
// fake in tests).
type rpcLink interface {
	Call(ctx context.Context, method string, params any) (json.RawMessage, error)
	Done() <-chan struct{}
	Kicked() bool
	Close()
}

// Dialer opens a session.
type Dialer func(ctx context.Context) (rpcLink, error)

// Event is a transcript entry broadcast to every WebSocket observer.
type Event struct {
	Line  string
	OK    bool
	Reply string // "OK", "OK <data>", or the error text
}

type request struct {
	ctx     context.Context
	line    string
	reply   chan result
	release bool
}

type result struct {
	data string
	err  error
}

// Owner holds the one JetKVM session, serializes every command onto it, and
// broadcasts a transcript. It connects lazily on the first RPC and keeps the
// session alive in the background.
//
// Reconnect policy: a dropped session is re-dialed in the background with
// exponential backoff (1 s up to 30 s). A session closed because another
// client took over (`otherSessionConnected`) is NOT re-dialed in the
// background -- doing so would fight a person's browser for the single
// session. The next explicit command reconnects, because that is intent.
type Owner struct {
	device string
	dial   Dialer

	mu      sync.Mutex
	link    rpcLink
	kicked  bool
	lastErr string
	nextTry time.Time
	backoff time.Duration

	connMu  sync.Mutex // serializes dialing
	session *Session
	reqs    chan request

	subMu sync.Mutex
	subs  map[chan Event]struct{}

	video *Video
}

// SetVideo attaches the video pipeline (nil disables /rfb).
func (o *Owner) SetVideo(v *Video) { o.video = v }

// Video returns the video pipeline, or nil.
func (o *Owner) Video() *Video { return o.video }

func (o *Owner) ensure() error {
	_, err := o.ensureLink()
	return err
}

func (o *Owner) linkState() (connected, kicked bool) {
	c, k, _ := o.Status()
	return c, k
}

// NewOwner returns an Owner for device (used only for status output).
func NewOwner(device string, dial Dialer) *Owner {
	o := &Owner{
		device: device,
		dial:   dial,
		reqs:   make(chan request, reqCap),
		subs:   map[chan Event]struct{}{},
	}
	o.session = NewSession(o)
	return o
}

// Start runs the worker and the background maintainer until ctx ends.
func (o *Owner) Start(ctx context.Context) {
	go o.worker(ctx)
	go o.maintain(ctx)
}

func (o *Owner) liveLink() rpcLink {
	o.mu.Lock()
	defer o.mu.Unlock()
	if o.link == nil {
		return nil
	}
	select {
	case <-o.link.Done():
		return nil
	default:
		return o.link
	}
}

// Connected reports whether a session is up.
func (o *Owner) Connected() bool { return o.liveLink() != nil }

// Status describes the connection for /status.
func (o *Owner) Status() (connected, kicked bool, lastErr string) {
	connected = o.liveLink() != nil
	o.mu.Lock()
	defer o.mu.Unlock()
	return connected, o.kicked, o.lastErr
}

func (o *Owner) ensureLink() (rpcLink, error) {
	if l := o.liveLink(); l != nil {
		return l, nil
	}
	o.connMu.Lock()
	defer o.connMu.Unlock()
	if l := o.liveLink(); l != nil {
		return l, nil
	}
	ctx, cancel := context.WithTimeout(context.Background(), dialTimeout)
	defer cancel()
	l, err := o.dial(ctx)
	o.mu.Lock()
	defer o.mu.Unlock()
	if err != nil {
		o.lastErr = err.Error()
		return nil, err
	}
	o.link, o.kicked, o.lastErr = l, false, ""
	o.backoff = 0
	slog.Info("JetKVM session established", "device", o.device)
	go o.watch(l)
	return l, nil
}

// watch notes how a session ended.
func (o *Owner) watch(l rpcLink) {
	<-l.Done()
	o.mu.Lock()
	defer o.mu.Unlock()
	if o.link != l {
		return
	}
	o.link = nil
	if l.Kicked() {
		o.kicked = true
		o.lastErr = ErrKicked.Error()
		slog.Warn("JetKVM session taken over by another client (a browser?); " +
			"not reconnecting until the next command")
		return
	}
	o.lastErr = "session dropped"
	o.nextTry = time.Now().Add(time.Second)
	slog.Warn("JetKVM session dropped; reconnecting in the background")
}

func (o *Owner) drop(l rpcLink) {
	l.Close()
}

// Call implements Caller on top of the live session, connecting first when
// there is none.
func (o *Owner) Call(method string, params any) (json.RawMessage, error) {
	l, err := o.ensureLink()
	if err != nil {
		return nil, err
	}
	ctx, cancel := context.WithTimeout(context.Background(), rpcTimeout)
	defer cancel()
	r, err := l.Call(ctx, method, params)
	var rpcErr *RPCError
	if err != nil && !errors.As(err, &rpcErr) {
		// Timeout or transport loss: the session is not trustworthy.
		o.drop(l)
	}
	return r, err
}

func (o *Owner) maintain(ctx context.Context) {
	t := time.NewTicker(time.Second)
	defer t.Stop()
	lastPing := time.Now()
	for {
		select {
		case <-ctx.Done():
			return
		case <-t.C:
		}
		if l := o.liveLink(); l != nil {
			if time.Since(lastPing) >= pingEvery {
				lastPing = time.Now()
				pctx, cancel := context.WithTimeout(ctx, rpcTimeout)
				_, err := l.Call(pctx, "ping", nil)
				cancel()
				if err != nil {
					var rpcErr *RPCError
					if !errors.As(err, &rpcErr) {
						slog.Warn("JetKVM keepalive ping failed; dropping session", "err", err)
						o.drop(l)
					}
				}
			}
			continue
		}
		o.mu.Lock()
		skip := o.kicked || time.Now().Before(o.nextTry)
		o.mu.Unlock()
		if skip {
			continue
		}
		if _, err := o.ensureLink(); err != nil {
			o.mu.Lock()
			if o.backoff == 0 {
				o.backoff = time.Second
			} else if o.backoff *= 2; o.backoff > maxBackoff {
				o.backoff = maxBackoff
			}
			o.nextTry = time.Now().Add(o.backoff)
			wait := o.backoff
			o.mu.Unlock()
			slog.Warn("JetKVM connect failed", "err", err, "retry_in", wait.String())
		}
		lastPing = time.Now()
	}
}

func (o *Owner) worker(ctx context.Context) {
	for {
		select {
		case <-ctx.Done():
			return
		case req := <-o.reqs:
			o.handle(req)
		}
	}
}

func (o *Owner) handle(req request) {
	if req.release {
		var err error
		if o.Connected() {
			err = o.session.release()
		}
		req.reply <- result{err: err}
		return
	}
	// A client that already gave up must not have its command injected late.
	if req.ctx.Err() != nil {
		req.reply <- result{err: req.ctx.Err()}
		return
	}
	data, err := safeExecute(o.session, req.line)
	o.broadcast(req.line, data, err)
	req.reply <- result{data: data, err: err}
}

func safeExecute(s *Session, line string) (data string, err error) {
	defer func() {
		if r := recover(); r != nil {
			err = fmt.Errorf("internal error: %v", r)
		}
	}()
	return executeLine(s, line)
}

// Send queues one command line and waits for its result.
func (o *Owner) Send(ctx context.Context, line string) (string, error) {
	ctx, cancel := context.WithTimeout(ctx, sendTimeout)
	defer cancel()
	// A CR/LF inside the line is refused before it is queued.
	if strings.ContainsAny(strings.TrimRight(line, "\r\n"), "\r\n") {
		return "", fmt.Errorf("command contains a newline: %q", line)
	}
	req := request{ctx: ctx, line: line, reply: make(chan result, 1)}
	select {
	case o.reqs <- req:
	case <-ctx.Done():
		return "", ctx.Err()
	}
	select {
	case r := <-req.reply:
		return r.data, r.err
	case <-ctx.Done():
		return "", errors.New("timed out waiting for the JetKVM command queue")
	}
}

// ReleaseForShutdown lets go of every held key and button on the target, if a
// session is up. The daemon is the only thing that remembers what it pressed.
func (o *Owner) ReleaseForShutdown(timeout time.Duration) error {
	req := request{reply: make(chan result, 1), release: true}
	timer := time.NewTimer(timeout)
	defer timer.Stop()
	select {
	case o.reqs <- req:
	case <-timer.C:
		return errors.New("release: queue full")
	}
	select {
	case r := <-req.reply:
		return r.err
	case <-timer.C:
		return errors.New("release: timed out")
	}
}

// Subscribe registers a transcript observer.
func (o *Owner) Subscribe() (<-chan Event, func()) {
	ch := make(chan Event, eventCap)
	o.subMu.Lock()
	o.subs[ch] = struct{}{}
	o.subMu.Unlock()
	return ch, func() {
		o.subMu.Lock()
		delete(o.subs, ch)
		o.subMu.Unlock()
	}
}

func (o *Owner) broadcast(line, data string, err error) {
	ev := Event{Line: line, OK: err == nil}
	switch {
	case err != nil:
		ev.Reply = err.Error()
	case data == "":
		ev.Reply = "OK"
	default:
		ev.Reply = "OK " + data
	}
	o.subMu.Lock()
	defer o.subMu.Unlock()
	for ch := range o.subs {
		select {
		case ch <- ev:
		default: // a lagging observer loses events rather than stalling the wire
		}
	}
}
