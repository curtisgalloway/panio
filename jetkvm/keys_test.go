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

import "testing"

func TestNameToKey(t *testing.T) {
	cases := map[string]key{
		"A": usage(0x04), "z": usage(0x1D), "ZERO": usage(0x27), "ONE": usage(0x1E),
		"ENTER": usage(0x28), "UP_ARROW": usage(0x52), "FORWARD_SLASH": usage(0x38),
		"F12": usage(0x45), "LEFT_CONTROL": mod(0x01), "ctrl": mod(0x01),
		"LEFT_GUI": mod(0x08), "RIGHT_ALT": mod(0x40),
	}
	for name, want := range cases {
		got, err := nameToKey(name)
		if err != nil || got != want {
			t.Errorf("%s: %v %v want %v", name, got, err, want)
		}
	}
	if _, err := nameToKey("NOPE"); err == nil {
		t.Error("unknown name must fail")
	}
}

func TestLockKeys(t *testing.T) {
	for _, n := range []string{"CAPS_LOCK", "SCROLL_LOCK", "NUM_LOCK"} {
		k, _ := nameToKey(n)
		if !k.isLock() {
			t.Errorf("%s should be a lock key", n)
		}
	}
	for _, n := range []string{"A", "LEFT_SHIFT"} {
		k, _ := nameToKey(n)
		if k.isLock() {
			t.Errorf("%s is not a lock key", n)
		}
	}
}

// Every printable ASCII character must be typeable on the US layout, and no
// two characters may share both usage and shift state.
func TestFullPrintableASCIIMapping(t *testing.T) {
	seen := map[[2]int]rune{}
	for c := rune(0x20); c < 0x7f; c++ {
		u, shift, err := charToUsage(c)
		if err != nil {
			t.Fatalf("%q: %v", c, err)
		}
		k := [2]int{int(u), 0}
		if shift {
			k[1] = 1
		}
		if prev, dup := seen[k]; dup {
			t.Fatalf("%q and %q map to the same key", prev, c)
		}
		seen[k] = c
	}
	if _, _, err := charToUsage('€'); err == nil {
		t.Error("non-US character must fail")
	}
}

func TestUSCharSpotChecks(t *testing.T) {
	type want struct {
		u     byte
		shift bool
	}
	for c, w := range map[rune]want{
		'a': {0x04, false}, 'A': {0x04, true}, '1': {0x1E, false}, '!': {0x1E, true},
		'/': {0x38, false}, '?': {0x38, true}, ' ': {0x2C, false}, '0': {0x27, false},
		')': {0x27, true}, '_': {0x2D, true}, '~': {0x35, true}, '\n': {0x28, false},
	} {
		u, shift, err := charToUsage(c)
		if err != nil || u != w.u || shift != w.shift {
			t.Errorf("%q: %#x %v %v", c, u, shift, err)
		}
	}
}
