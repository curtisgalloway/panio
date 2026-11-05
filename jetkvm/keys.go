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
	"fmt"
	"strings"
)

// Modifier bits of the HID keyboard report's first byte.
const (
	modLeftControl  byte = 0x01
	modLeftShift    byte = 0x02
	modLeftAlt      byte = 0x04
	modLeftGUI      byte = 0x08
	modRightControl byte = 0x10
	modRightShift   byte = 0x20
	modRightAlt     byte = 0x40
	modRightGUI     byte = 0x80
)

// key is a resolved key name: a modifier bit or an ordinary HID usage that
// occupies one of the six key slots of a keyboard report.
type key struct {
	modifier bool
	value    byte
}

func usage(v byte) key { return key{value: v} }
func mod(v byte) key   { return key{modifier: true, value: v} }

// isLock reports whether the key is Caps, Scroll or Num Lock, which hosts
// debounce and so need a longer hold.
func (k key) isLock() bool {
	return !k.modifier && (k.value == 0x39 || k.value == 0x47 || k.value == 0x53)
}

var namedKeys = map[string]key{
	"ZERO": usage(0x27), "ONE": usage(0x1E), "TWO": usage(0x1F),
	"THREE": usage(0x20), "FOUR": usage(0x21), "FIVE": usage(0x22),
	"SIX": usage(0x23), "SEVEN": usage(0x24), "EIGHT": usage(0x25),
	"NINE": usage(0x26),

	"ENTER": usage(0x28), "RETURN": usage(0x28),
	"ESCAPE": usage(0x29), "ESC": usage(0x29),
	"BACKSPACE": usage(0x2A), "TAB": usage(0x2B),
	"SPACE": usage(0x2C), "SPACEBAR": usage(0x2C),
	"DELETE": usage(0x4C), "DEL": usage(0x4C),
	"INSERT": usage(0x49), "HOME": usage(0x4A), "END": usage(0x4D),
	"PAGE_UP": usage(0x4B), "PAGE_DOWN": usage(0x4E),

	"RIGHT_ARROW": usage(0x4F), "LEFT_ARROW": usage(0x50),
	"DOWN_ARROW": usage(0x51), "UP_ARROW": usage(0x52),

	"MINUS": usage(0x2D), "EQUALS": usage(0x2E),
	"LEFT_BRACKET": usage(0x2F), "RIGHT_BRACKET": usage(0x30),
	"BACKSLASH": usage(0x31), "SEMICOLON": usage(0x33),
	"QUOTE": usage(0x34), "GRAVE_ACCENT": usage(0x35),
	"COMMA": usage(0x36), "PERIOD": usage(0x37),
	"FORWARD_SLASH": usage(0x38),

	"CAPS_LOCK": usage(0x39), "PRINT_SCREEN": usage(0x46),
	"SCROLL_LOCK": usage(0x47), "PAUSE": usage(0x48),
	"KEYPAD_NUMLOCK": usage(0x53), "NUM_LOCK": usage(0x53),
	"APPLICATION": usage(0x65), "MENU": usage(0x65),

	"F1": usage(0x3A), "F2": usage(0x3B), "F3": usage(0x3C),
	"F4": usage(0x3D), "F5": usage(0x3E), "F6": usage(0x3F),
	"F7": usage(0x40), "F8": usage(0x41), "F9": usage(0x42),
	"F10": usage(0x43), "F11": usage(0x44), "F12": usage(0x45),

	"LEFT_CONTROL": mod(modLeftControl), "CONTROL": mod(modLeftControl),
	"CTRL":       mod(modLeftControl),
	"LEFT_SHIFT": mod(modLeftShift), "SHIFT": mod(modLeftShift),
	"LEFT_ALT": mod(modLeftAlt), "ALT": mod(modLeftAlt),
	"LEFT_GUI": mod(modLeftGUI), "GUI": mod(modLeftGUI),
	"WINDOWS": mod(modLeftGUI), "COMMAND": mod(modLeftGUI),
	"RIGHT_CONTROL": mod(modRightControl), "RIGHT_SHIFT": mod(modRightShift),
	"RIGHT_ALT": mod(modRightAlt), "RIGHT_GUI": mod(modRightGUI),
}

// nameToKey resolves an adafruit_hid Keycode name (case-insensitive). An
// unknown name is an error: the protocol mandates ERR, never a guess.
func nameToKey(name string) (key, error) {
	n := strings.ToUpper(name)
	if len(n) == 1 && n[0] >= 'A' && n[0] <= 'Z' {
		return usage(0x04 + n[0] - 'A'), nil
	}
	if k, ok := namedKeys[n]; ok {
		return k, nil
	}
	return key{}, fmt.Errorf("unknown key name: %s", name)
}

var shiftedDigitRow = map[rune]byte{
	'!': 0x1E, '@': 0x1F, '#': 0x20, '$': 0x21, '%': 0x22,
	'^': 0x23, '&': 0x24, '*': 0x25, '(': 0x26, ')': 0x27,
}

var plainPunct = map[rune]byte{
	'-': 0x2D, '=': 0x2E, '[': 0x2F, ']': 0x30, '\\': 0x31, ';': 0x33,
	'\'': 0x34, '`': 0x35, ',': 0x36, '.': 0x37, '/': 0x38,
}

var shiftedPunct = map[rune]byte{
	'_': 0x2D, '+': 0x2E, '{': 0x2F, '}': 0x30, '|': 0x31, ':': 0x33,
	'"': 0x34, '~': 0x35, '<': 0x36, '>': 0x37, '?': 0x38,
}

// charToUsage maps one character to (usage, needsShift) for a US layout.
func charToUsage(c rune) (byte, bool, error) {
	switch {
	case c == '\n':
		return 0x28, false, nil
	case c == '\t':
		return 0x2B, false, nil
	case c == ' ':
		return 0x2C, false, nil
	case c >= 'a' && c <= 'z':
		return 0x04 + byte(c-'a'), false, nil
	case c >= 'A' && c <= 'Z':
		return 0x04 + byte(c-'A'), true, nil
	case c >= '1' && c <= '9':
		return 0x1E + byte(c-'1'), false, nil
	case c == '0':
		return 0x27, false, nil
	}
	if u, ok := plainPunct[c]; ok {
		return u, false, nil
	}
	if u, ok := shiftedDigitRow[c]; ok {
		return u, true, nil
	}
	if u, ok := shiftedPunct[c]; ok {
		return u, true, nil
	}
	return 0, false, fmt.Errorf("cannot type character %q (US layout only)", c)
}
