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
	"strings"
	"testing"
)

// fakeCaller records every RPC as "method {json params}".
type fakeCaller struct {
	calls  []string
	failAt int // 1-based call index that fails; 0 never
	result map[string]string
}

func (f *fakeCaller) Call(method string, params any) (json.RawMessage, error) {
	b, _ := json.Marshal(params)
	f.calls = append(f.calls, method+" "+string(b))
	if f.failAt > 0 && len(f.calls) == f.failAt {
		return nil, errors.New("link lost")
	}
	if r, ok := f.result[method]; ok {
		return json.RawMessage(r), nil
	}
	return json.RawMessage("null"), nil
}

func newTestSession() (*Session, *fakeCaller) {
	f := &fakeCaller{result: map[string]string{"ping": `"pong"`}}
	s := NewSession(f)
	s.t = timing{}
	return s, f
}

func mustExec(t *testing.T, s *Session, line string) string {
	t.Helper()
	data, err := executeLine(s, line)
	if err != nil {
		t.Fatalf("%q: %v", line, err)
	}
	return data
}

func expectCalls(t *testing.T, f *fakeCaller, want ...string) {
	t.Helper()
	if strings.Join(f.calls, "\n") != strings.Join(want, "\n") {
		t.Fatalf("calls:\n%s\nwant:\n%s", strings.Join(f.calls, "\n"), strings.Join(want, "\n"))
	}
}

func TestTypeComposesShiftedAndPlainReports(t *testing.T) {
	s, f := newTestSession()
	mustExec(t, s, "type Hi!")
	expectCalls(t, f,
		`keyboardReport {"keys":[11],"modifier":2}`, // H = shift+h
		`keyboardReport {"keys":[],"modifier":0}`,
		`keyboardReport {"keys":[12],"modifier":0}`, // i
		`keyboardReport {"keys":[],"modifier":0}`,
		`keyboardReport {"keys":[30],"modifier":2}`, // ! = shift+1
		`keyboardReport {"keys":[],"modifier":0}`,
	)
}

func TestTypeKeepsTrailingWhitespaceAndHash(t *testing.T) {
	s, f := newTestSession()
	mustExec(t, s, "type #  ")
	if len(f.calls) != 6 { // '#', ' ', ' ' -> press+release each
		t.Fatalf("got %d calls: %v", len(f.calls), f.calls)
	}
}

func TestTypeRefusesWholeStringOnBadCharacter(t *testing.T) {
	s, f := newTestSession()
	if _, err := executeLine(s, "type ab€"); err == nil {
		t.Fatal("expected an error for a non-US character")
	}
	if len(f.calls) != 0 {
		t.Fatalf("a partially typed string was sent: %v", f.calls)
	}
}

func TestTypeCeiling(t *testing.T) {
	if checkTypeLen(strings.Repeat("a", maxTypeChars)) != nil {
		t.Fatal("exactly the cap must be accepted")
	}
	if checkTypeLen(strings.Repeat("a", maxTypeChars+1)) == nil {
		t.Fatal("one over the cap must be refused")
	}
	if checkTypeLen(strings.Repeat("é", maxTypeChars)) != nil {
		t.Fatal("the cap counts characters, not bytes")
	}
}

func TestEmbeddedNewlineIsRefused(t *testing.T) {
	s, f := newTestSession()
	if _, err := executeLine(s, "type a\nb"); err == nil {
		t.Fatal("an embedded newline must be refused")
	}
	if _, err := executeLine(s, "type ok\r\n"); err != nil {
		t.Fatalf("a trailing terminator is stripped: %v", err)
	}
	if len(f.calls) != 4 {
		t.Fatalf("calls: %v", f.calls)
	}
}

func TestKeyComboAndHeldState(t *testing.T) {
	s, f := newTestSession()
	mustExec(t, s, "key ENTER")
	mustExec(t, s, "combo LEFT_CONTROL LEFT_ALT DELETE")
	mustExec(t, s, "down LEFT_SHIFT")
	mustExec(t, s, "down A")
	mustExec(t, s, "up A")
	mustExec(t, s, "releaseall")
	expectCalls(t, f,
		`keyboardReport {"keys":[40],"modifier":0}`,
		`keyboardReport {"keys":[],"modifier":0}`,
		`keyboardReport {"keys":[76],"modifier":5}`,
		`keyboardReport {"keys":[],"modifier":0}`,
		`keyboardReport {"keys":[],"modifier":2}`,
		`keyboardReport {"keys":[4],"modifier":2}`,
		`keyboardReport {"keys":[],"modifier":2}`,
		`keyboardReport {"keys":[],"modifier":0}`,
		`relMouseReport {"buttons":0,"dx":0,"dy":0}`,
	)
}

func TestHeldModifierAppliesToTypedText(t *testing.T) {
	s, f := newTestSession()
	mustExec(t, s, "down LEFT_CONTROL")
	mustExec(t, s, "type c")
	if f.calls[1] != `keyboardReport {"keys":[6],"modifier":1}` || f.calls[2] != `keyboardReport {"keys":[],"modifier":1}` {
		t.Fatalf("calls: %v", f.calls)
	}
}

func TestComboRefusesMoreThanSixKeys(t *testing.T) {
	s, f := newTestSession()
	if _, err := executeLine(s, "combo A B C D E F G"); err == nil {
		t.Fatal("seven key slots must be refused, not truncated")
	}
	if len(f.calls) != 0 {
		t.Fatalf("calls: %v", f.calls)
	}
	mustExec(t, s, "combo A B C D E F")
	// Modifiers have their own byte and do not count.
	mustExec(t, s, "combo LEFT_CONTROL LEFT_SHIFT A B C D E F")
	// Counting keys already held via down.
	mustExec(t, s, "down A")
	if _, err := executeLine(s, "combo B C D E F G"); err == nil {
		t.Fatal("held keys count against the six slots")
	}
}

func TestDownRefusesSeventhKey(t *testing.T) {
	s, _ := newTestSession()
	for _, k := range []string{"A", "B", "C", "D", "E", "F"} {
		mustExec(t, s, "down "+k)
	}
	if _, err := executeLine(s, "down G"); err == nil {
		t.Fatal("a seventh held key must be refused")
	}
}

func TestUnknownKeyAndCommandAreErrors(t *testing.T) {
	s, _ := newTestSession()
	for _, line := range []string{"key NOPE", "frobnicate", "key", "move 1", "move a b", "scroll x", "click nobutton"} {
		if _, err := executeLine(s, line); err == nil {
			t.Errorf("%q should fail", line)
		}
	}
}

func TestMoveSplitsIntoInt8Steps(t *testing.T) {
	s, f := newTestSession()
	mustExec(t, s, "move 300 -50")
	expectCalls(t, f,
		`relMouseReport {"buttons":0,"dx":127,"dy":-50}`,
		`relMouseReport {"buttons":0,"dx":127,"dy":0}`,
		`relMouseReport {"buttons":0,"dx":46,"dy":0}`,
	)
}

func TestMoveClampsTotalTravel(t *testing.T) {
	s, f := newTestSession()
	mustExec(t, s, "move 1000000 0")
	moved := 0
	for _, c := range f.calls {
		var p struct{ DX int }
		_, js, _ := strings.Cut(c, " ")
		var m map[string]int
		_ = json.Unmarshal([]byte(js), &m)
		p.DX = m["dx"]
		moved += p.DX
	}
	if moved != maxRelTotal {
		t.Fatalf("moved %d, want clamp to %d", moved, maxRelTotal)
	}
}

func TestMoveabsClamps(t *testing.T) {
	s, f := newTestSession()
	mustExec(t, s, "moveabs -5 99999")
	expectCalls(t, f, `absMouseReport {"buttons":0,"x":0,"y":32767}`)
	if clampAbs(-1) != 0 || clampAbs(absMax+1) != absMax || clampAbs(7) != 7 {
		t.Fatal("clampAbs bounds")
	}
}

func TestClickAndHeldButtons(t *testing.T) {
	s, f := newTestSession()
	mustExec(t, s, "click")
	mustExec(t, s, "mdown right")
	mustExec(t, s, "moveabs 1 2")
	mustExec(t, s, "mup right")
	expectCalls(t, f,
		`relMouseReport {"buttons":1,"dx":0,"dy":0}`,
		`relMouseReport {"buttons":0,"dx":0,"dy":0}`,
		`relMouseReport {"buttons":2,"dx":0,"dy":0}`,
		`absMouseReport {"buttons":2,"x":1,"y":2}`,
		`relMouseReport {"buttons":0,"dx":0,"dy":0}`,
	)
}

func TestScrollSplits(t *testing.T) {
	s, f := newTestSession()
	mustExec(t, s, "scroll -200")
	expectCalls(t, f,
		`wheelReport {"wheelX":0,"wheelY":-127}`,
		`wheelReport {"wheelX":0,"wheelY":-73}`,
	)
}

func TestPingChecksPong(t *testing.T) {
	s, f := newTestSession()
	mustExec(t, s, "ping")
	f.result["ping"] = `"nope"`
	if _, err := executeLine(s, "ping"); err == nil {
		t.Fatal("a wrong ping reply must be an error")
	}
}

func TestVersionNeedsNoDevice(t *testing.T) {
	s, f := newTestSession()
	got := mustExec(t, s, "version")
	if got != "1 jetkvm/"+version+" moveabs" || len(f.calls) != 0 {
		t.Fatalf("version %q calls %v", got, f.calls)
	}
}

func TestInfoQueriesDeviceState(t *testing.T) {
	s, f := newTestSession()
	f.result["getUSBState"] = `"configured"`
	f.result["getVideoState"] = `{"ready":true}`
	f.result["getKeyboardLedState"] = `{"num_lock":false}`
	got := mustExec(t, s, "info")
	want := `usb="configured" video={"ready":true} leds={"num_lock":false}`
	if got != want {
		t.Fatalf("info %q want %q", got, want)
	}
}

func TestTransportErrorMidTypeReleasesHeldState(t *testing.T) {
	s, f := newTestSession()
	f.failAt = 3 // second character's press
	if _, err := executeLine(s, "type ab"); err == nil {
		t.Fatal("expected the transport error")
	}
	last := f.calls[len(f.calls)-1]
	if last != `keyboardReport {"keys":[],"modifier":0}` {
		t.Fatalf("a best-effort release must follow a failed press, got %s", last)
	}
}

func TestParseSequence(t *testing.T) {
	steps, err := parseSequence("# boot\ntype root\nkey ENTER\ndelay 500\n\nsleep 1.5\nmove 300 -50\n")
	if err != nil {
		t.Fatal(err)
	}
	want := []step{
		{cmd: "type root"}, {cmd: "key ENTER"},
		{delay: 0.5, isDelay: true}, {delay: 1.5, isDelay: true},
		{cmd: "move 300 -50"},
	}
	if len(steps) != len(want) {
		t.Fatalf("%v", steps)
	}
	for i := range want {
		if steps[i] != want[i] {
			t.Fatalf("step %d: %v want %v", i, steps[i], want[i])
		}
	}
}

func TestParseSequenceKeepsHashAndTrailingSpaces(t *testing.T) {
	steps, _ := parseSequence("type issue #42\n  type hi  \n")
	if steps[0].cmd != "type issue #42" || steps[1].cmd != "type hi  " {
		t.Fatalf("%v", steps)
	}
	if s, _ := parseSequence("   \n\t\n"); len(s) != 0 {
		t.Fatal("blank lines are skipped")
	}
}

func TestDelayAndSleepAreBounded(t *testing.T) {
	for _, bad := range []string{
		"delay -1", "sleep -0.5", "sleep inf", "delay inf", "sleep nan",
		"delay NaN", "sleep 3600.5", "delay 3600001", "sleep", "sleep x",
	} {
		if _, err := parseSequence(bad); err == nil {
			t.Errorf("%q must be refused", bad)
		}
	}
	steps, err := parseSequence("sleep 3600\ndelay 3600000\nsleep 0\n")
	if err != nil || len(steps) != 3 || steps[0].delay != 3600 || steps[1].delay != 3600 || steps[2].delay != 0 {
		t.Fatalf("%v %v", steps, err)
	}
}
