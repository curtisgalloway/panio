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
	"encoding/json"
	"errors"
	"fmt"
	"strings"
	"time"
	"unicode/utf8"
)

// Limits shared with the other hid helpers (docs/dev/hid-serial-protocol.md).
const (
	// maxTypeChars caps the text of one `type` command.
	maxTypeChars = 4096
	// maxRelTotal caps one move/scroll call's travel per axis.
	maxRelTotal = 32767
	// absMax is the moveabs logical maximum, which is also JetKVM's own
	// absolute-axis range.
	absMax = 32767
	// maxKeys is the number of key slots in a keyboard report.
	maxKeys = 6
)

// Caller is the JSON-RPC transport a Session drives.
type Caller interface {
	Call(method string, params any) (json.RawMessage, error)
}

// timing holds the pauses between reports. Tests zero it.
type timing struct {
	hold      time.Duration // key held during a tap/combo
	lockHold  time.Duration // same, for Caps/Num/Scroll Lock
	typeGap   time.Duration // between typed characters
	clickHold time.Duration // mouse button held during a click
	relGap    time.Duration // between split relative reports
}

var defaultTiming = timing{
	hold:      30 * time.Millisecond,
	lockHold:  200 * time.Millisecond,
	typeGap:   5 * time.Millisecond,
	clickHold: 50 * time.Millisecond,
	relGap:    4 * time.Millisecond,
}

// Session composes keyboard and mouse reports on top of a Caller. It owns the
// held-key and held-button state, so down/up/mdown/mup survive across
// commands (and across reconnects, since the target's USB gadget does).
//
// A Session is not safe for concurrent use; the daemon's queue serializes it.
type Session struct {
	c       Caller
	t       timing
	mods    byte
	held    []byte
	buttons byte
}

// NewSession returns a Session over c using the real pauses.
func NewSession(c Caller) *Session { return &Session{c: c, t: defaultTiming} }

func (s *Session) sleep(d time.Duration) {
	if d > 0 {
		time.Sleep(d)
	}
}

func (s *Session) keyboardReport(mods byte, keys []byte) error {
	list := make([]int, 0, len(keys))
	for _, k := range keys {
		list = append(list, int(k))
	}
	_, err := s.c.Call("keyboardReport", map[string]any{"modifier": mods, "keys": list})
	return err
}

// sendHeld sends the report for the current held state.
func (s *Session) sendHeld() error { return s.keyboardReport(s.mods, s.held) }

func contains(keys []byte, k byte) bool {
	for _, v := range keys {
		if v == k {
			return true
		}
	}
	return false
}

func without(keys []byte, k byte) []byte {
	out := make([]byte, 0, len(keys))
	for _, v := range keys {
		if v != k {
			out = append(out, v)
		}
	}
	return out
}

func (s *Session) keyDown(k key) error {
	if k.modifier {
		s.mods |= k.value
		return s.sendHeld()
	}
	if !contains(s.held, k.value) {
		if len(s.held) >= maxKeys {
			return fmt.Errorf("cannot hold more than %d keys at once", maxKeys)
		}
		s.held = append(s.held, k.value)
	}
	return s.sendHeld()
}

func (s *Session) keyUp(k key) error {
	if k.modifier {
		s.mods &^= k.value
	} else {
		s.held = without(s.held, k.value)
	}
	return s.sendHeld()
}

func (s *Session) releaseAll() error {
	s.mods = 0
	s.held = nil
	s.buttons = 0
	if err := s.sendHeld(); err != nil {
		return err
	}
	return s.relMouse(0, 0)
}

// combo presses every key at once on top of what is already held, holds, then
// restores the held state. More than six key slots is refused, not truncated.
func (s *Session) combo(keys []key) error {
	mods := s.mods
	usages := append([]byte(nil), s.held...)
	lock := false
	for _, k := range keys {
		if k.modifier {
			mods |= k.value
			continue
		}
		if !contains(usages, k.value) {
			usages = append(usages, k.value)
		}
		lock = lock || k.isLock()
	}
	if len(usages) > maxKeys {
		return fmt.Errorf("combo needs %d key slots; a report has %d", len(usages), maxKeys)
	}
	if err := s.keyboardReport(mods, usages); err != nil {
		_ = s.sendHeld()
		return err
	}
	if lock {
		s.sleep(s.t.lockHold)
	} else {
		s.sleep(s.t.hold)
	}
	return s.sendHeld()
}

func (s *Session) tap(k key) error { return s.combo([]key{k}) }

func checkTypeLen(text string) error {
	if n := utf8.RuneCountInString(text); n > maxTypeChars {
		return fmt.Errorf("type text is %d characters; the limit is %d", n, maxTypeChars)
	}
	return nil
}

// typeText types literal text on a US layout, on top of any held modifiers.
// The whole string is validated first so an unrepresentable character refuses
// the command instead of leaving a half-typed line on the target.
func (s *Session) typeText(text string) error {
	if err := checkTypeLen(text); err != nil {
		return err
	}
	type stroke struct {
		usage byte
		shift bool
	}
	var strokes []stroke
	for _, c := range text {
		u, shift, err := charToUsage(c)
		if err != nil {
			return err
		}
		strokes = append(strokes, stroke{u, shift})
	}
	for i, st := range strokes {
		mods := s.mods
		if st.shift {
			mods |= modLeftShift
		}
		keys := s.held
		if contains(keys, st.usage) {
			// Pressing an already-held usage would repeat the report on the
			// wire; release it first so the press is a real edge.
			if err := s.keyboardReport(mods, without(keys, st.usage)); err != nil {
				return err
			}
			keys = without(keys, st.usage)
		}
		if len(keys) >= maxKeys {
			return fmt.Errorf("cannot type with %d keys already held", len(keys))
		}
		press := append(append([]byte(nil), keys...), st.usage)
		if err := s.keyboardReport(mods, press); err != nil {
			_ = s.sendHeld()
			return fmt.Errorf("typing character %d: %w", i+1, err)
		}
		if err := s.sendHeld(); err != nil {
			return fmt.Errorf("typing character %d: %w", i+1, err)
		}
		s.sleep(s.t.typeGap)
	}
	return nil
}

func buttonMask(name string) (byte, error) {
	switch strings.ToLower(name) {
	case "left":
		return 0x01, nil
	case "right":
		return 0x02, nil
	case "middle":
		return 0x04, nil
	}
	return 0, fmt.Errorf("unknown mouse button: %s (left, right, middle)", name)
}

// relMouse sends one relative report with the current buttons.
func (s *Session) relMouse(dx, dy int) error {
	_, err := s.c.Call("relMouseReport", map[string]any{
		"dx": dx, "dy": dy, "buttons": s.buttons,
	})
	return err
}

func clampRelTotal(v int) int {
	if v > maxRelTotal {
		return maxRelTotal
	}
	if v < -maxRelTotal {
		return -maxRelTotal
	}
	return v
}

func clampStep(v int) int {
	if v > 127 {
		return 127
	}
	if v < -127 {
		return -127
	}
	return v
}

// moveRel moves by (dx, dy), split into int8 steps; each axis is clamped to
// maxRelTotal per call.
func (s *Session) moveRel(dx, dy int) error {
	dx, dy = clampRelTotal(dx), clampRelTotal(dy)
	for dx != 0 || dy != 0 {
		sx, sy := clampStep(dx), clampStep(dy)
		if err := s.relMouse(sx, sy); err != nil {
			return err
		}
		dx -= sx
		dy -= sy
		if dx != 0 || dy != 0 {
			s.sleep(s.t.relGap)
		}
	}
	return nil
}

func clampAbs(v int) int {
	if v < 0 {
		return 0
	}
	if v > absMax {
		return absMax
	}
	return v
}

func (s *Session) moveAbs(x, y int) error {
	_, err := s.c.Call("absMouseReport", map[string]any{
		"x": clampAbs(x), "y": clampAbs(y), "buttons": s.buttons,
	})
	return err
}

// click presses and releases a button through the relative report with zero
// motion, so it lands wherever the pointer already is (a preceding moveabs,
// even from another process, chooses the spot).
func (s *Session) click(button string) error {
	mask, err := buttonMask(button)
	if err != nil {
		return err
	}
	pressed := s.buttons
	s.buttons |= mask
	if err := s.relMouse(0, 0); err != nil {
		s.buttons = pressed
		return err
	}
	s.sleep(s.t.clickHold)
	s.buttons = pressed
	return s.relMouse(0, 0)
}

func (s *Session) mouseDown(button string) error {
	mask, err := buttonMask(button)
	if err != nil {
		return err
	}
	s.buttons |= mask
	return s.relMouse(0, 0)
}

func (s *Session) mouseUp(button string) error {
	mask, err := buttonMask(button)
	if err != nil {
		return err
	}
	s.buttons &^= mask
	return s.relMouse(0, 0)
}

// scroll turns the wheel; positive is up. Split into int8 steps.
func (s *Session) scroll(amount int) error {
	amount = clampRelTotal(amount)
	for amount != 0 {
		step := clampStep(amount)
		if _, err := s.c.Call("wheelReport", map[string]any{"wheelY": step, "wheelX": 0}); err != nil {
			return err
		}
		amount -= step
		if amount != 0 {
			s.sleep(s.t.relGap)
		}
	}
	return nil
}

// release lets go of every key and button, for shutdown.
func (s *Session) release() error { return s.releaseAll() }

func rawString(r json.RawMessage) string {
	var buf strings.Builder
	buf.Write(r)
	return buf.String()
}

// info reports device state: USB, video and keyboard LEDs.
func (s *Session) info() (string, error) {
	var parts []string
	for _, q := range []struct{ label, method string }{
		{"usb", "getUSBState"},
		{"video", "getVideoState"},
		{"leds", "getKeyboardLedState"},
	} {
		r, err := s.c.Call(q.method, nil)
		if err != nil {
			return "", fmt.Errorf("%s: %w", q.method, err)
		}
		parts = append(parts, q.label+"="+compactJSON(r))
	}
	return strings.Join(parts, " "), nil
}

func compactJSON(r json.RawMessage) string {
	var v any
	if json.Unmarshal(r, &v) != nil {
		return rawString(r)
	}
	b, err := json.Marshal(v)
	if err != nil {
		return rawString(r)
	}
	return string(b)
}

// versionReply is the `version` reply data: protocol version, implementation
// id and capabilities. JetKVM has a true absolute pointer, so moveabs.
const versionReply = "1 jetkvm/" + version + " moveabs"

// executeLine runs one protocol command line against s and returns the OK
// reply data (empty for a bare OK). It is the single backend for CLI
// subcommands, `run` files and the daemon.
//
// One trailing line terminator is stripped; a CR/LF anywhere else is refused
// rather than typed as Enter. `type` text is the remainder after the one
// separator, verbatim; every other verb takes whitespace-separated tokens.
func executeLine(s *Session, line string) (string, error) {
	line = strings.TrimRight(line, "\r\n")
	if strings.ContainsAny(line, "\r\n") {
		return "", fmt.Errorf("command contains a newline: %q", line)
	}
	line = strings.TrimLeft(line, " \t")
	head, rest, _ := strings.Cut(line, " ")
	rest = strings.TrimSpace(rest)
	switch strings.ToLower(head) {
	case "type":
		return "", s.typeText(strings.TrimPrefix(strings.TrimPrefix(line, head), " "))
	case "key":
		name, err := oneArg(rest, "key")
		if err != nil {
			return "", err
		}
		k, err := nameToKey(name)
		if err != nil {
			return "", err
		}
		return "", s.tap(k)
	case "combo":
		var chord []key
		for _, name := range strings.Fields(rest) {
			k, err := nameToKey(name)
			if err != nil {
				return "", err
			}
			chord = append(chord, k)
		}
		if len(chord) == 0 {
			return "", errors.New("combo needs at least one key name")
		}
		return "", s.combo(chord)
	case "down", "up":
		name, err := oneArg(rest, strings.ToLower(head))
		if err != nil {
			return "", err
		}
		k, err := nameToKey(name)
		if err != nil {
			return "", err
		}
		if strings.EqualFold(head, "down") {
			return "", s.keyDown(k)
		}
		return "", s.keyUp(k)
	case "releaseall", "release":
		return "", s.releaseAll()
	case "move":
		dx, dy, err := twoInts(rest, "move")
		if err != nil {
			return "", err
		}
		return "", s.moveRel(dx, dy)
	case "moveabs":
		x, y, err := twoInts(rest, "moveabs")
		if err != nil {
			return "", err
		}
		return "", s.moveAbs(x, y)
	case "click":
		return "", s.click(buttonOrDefault(rest))
	case "mdown":
		return "", s.mouseDown(buttonOrDefault(rest))
	case "mup":
		return "", s.mouseUp(buttonOrDefault(rest))
	case "scroll":
		arg, err := oneArg(rest, "scroll")
		if err != nil {
			return "", err
		}
		n, err := parseInt(arg)
		if err != nil {
			return "", fmt.Errorf("scroll amount must be an integer: %q", rest)
		}
		return "", s.scroll(n)
	case "ping":
		r, err := s.c.Call("ping", nil)
		if err != nil {
			return "", err
		}
		if got := compactJSON(r); got != `"pong"` {
			return "", fmt.Errorf("unexpected ping reply: %s", got)
		}
		return "", nil
	case "version":
		return versionReply, nil
	case "info":
		return s.info()
	}
	return "", fmt.Errorf("unknown command: %s", strings.ToLower(head))
}

func oneArg(rest, verb string) (string, error) {
	f := strings.Fields(rest)
	if len(f) == 0 {
		return "", fmt.Errorf("%s needs an argument", verb)
	}
	return f[0], nil
}

func twoInts(rest, verb string) (int, int, error) {
	f := strings.Fields(rest)
	if len(f) >= 2 {
		a, ea := parseInt(f[0])
		b, eb := parseInt(f[1])
		if ea == nil && eb == nil {
			return a, b, nil
		}
	}
	return 0, 0, fmt.Errorf("%s needs two integer arguments: %q", verb, rest)
}

func buttonOrDefault(rest string) string {
	f := strings.Fields(rest)
	if len(f) == 0 {
		return "left"
	}
	return f[0]
}

// step is one step of a command file.
type step struct {
	cmd     string  // a protocol command line, when not a delay
	delay   float64 // a pause in seconds, when isDelay
	isDelay bool
}

// maxPauseSecs is the longest pause a sequence file may ask for.
const maxPauseSecs = 3600.0

// parseSequence parses a command file: non-blank, non-# lines are commands;
// `delay <ms>` and `sleep <seconds>` are timing directives, each finite and
// between 0 and one hour. Command lines keep trailing whitespace (part of a
// `type` line's text); only leading whitespace is removed.
func parseSequence(text string) ([]step, error) {
	var steps []step
	for _, raw := range strings.Split(text, "\n") {
		raw = strings.TrimSuffix(raw, "\r")
		line := strings.TrimLeft(raw, " \t")
		if strings.TrimSpace(line) == "" || strings.HasPrefix(line, "#") {
			continue
		}
		head, rest, _ := strings.Cut(line, " ")
		value := ""
		if f := strings.Fields(rest); len(f) > 0 {
			value = f[0]
		}
		switch strings.ToLower(head) {
		case "delay":
			d, err := pauseSecs(value, 1000, "delay", rest)
			if err != nil {
				return nil, err
			}
			steps = append(steps, step{delay: d, isDelay: true})
		case "sleep":
			d, err := pauseSecs(value, 1, "sleep", rest)
			if err != nil {
				return nil, err
			}
			steps = append(steps, step{delay: d, isDelay: true})
		default:
			steps = append(steps, step{cmd: line})
		}
	}
	return steps, nil
}

func pauseSecs(value string, perSecond float64, what, rest string) (float64, error) {
	v, err := parseFloat(value)
	if err != nil {
		return 0, fmt.Errorf("invalid %s value: %q", what, rest)
	}
	secs := v / perSecond
	if secs != secs || secs < 0 || secs > maxPauseSecs {
		return 0, fmt.Errorf("%s must be between 0 and %v seconds: %q", what, maxPauseSecs, rest)
	}
	if secs == 0 {
		secs = 0
	}
	return secs, nil
}
