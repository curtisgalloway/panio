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
	"errors"
	"os"
	"path/filepath"
	"runtime"
	"strings"
	"testing"
	"time"
)

func writeSecretFile(t *testing.T, content string, mode os.FileMode) string {
	t.Helper()
	p := filepath.Join(t.TempDir(), "pw")
	if err := os.WriteFile(p, []byte(content), mode); err != nil {
		t.Fatal(err)
	}
	return p
}

func isNotConfigured(err error) bool {
	var nc *NotConfigured
	return errors.As(err, &nc)
}

func TestPasswordEnvWinsOverFileAndCommand(t *testing.T) {
	f := writeSecretFile(t, "from-file\n", 0o600)
	got, err := resolvePasswordWith("from-env", PasswordSources{File: f, Command: "echo from-cmd"}, time.Second)
	if err != nil || got != "from-env" {
		t.Fatalf("%q %v", got, err)
	}
}

func TestPasswordFileWinsOverCommand(t *testing.T) {
	f := writeSecretFile(t, "from-file\n", 0o600)
	got, err := resolvePasswordWith("", PasswordSources{File: f, Command: "echo from-cmd"}, time.Second)
	if err != nil || got != "from-file" {
		t.Fatalf("%q %v", got, err)
	}
}

func TestPasswordCommandIsLastResort(t *testing.T) {
	if runtime.GOOS == "windows" {
		t.Skip("uses sh")
	}
	got, err := resolvePasswordWith("", PasswordSources{Command: "echo from-cmd"}, 5*time.Second)
	if err != nil || got != "from-cmd" {
		t.Fatalf("%q %v", got, err)
	}
}

func TestPasswordDropsExactlyOneTrailingNewline(t *testing.T) {
	for in, want := range map[string]string{
		"pw\n": "pw", "pw\r\n": "pw", "pw": "pw", "pw\n\n": "pw\n", " pw ": " pw ",
	} {
		f := writeSecretFile(t, in, 0o600)
		got, err := resolvePasswordWith("", PasswordSources{File: f}, time.Second)
		if err != nil || got != want {
			t.Errorf("%q: %q %v want %q", in, got, err, want)
		}
	}
}

func TestPasswordFailuresAreNotConfigured(t *testing.T) {
	empty := writeSecretFile(t, "\n", 0o600)
	cases := map[string]PasswordSources{
		"nothing set":  {},
		"missing file": {File: filepath.Join(t.TempDir(), "absent")},
		"empty file":   {File: empty},
	}
	if runtime.GOOS != "windows" {
		cases["failing command"] = PasswordSources{Command: "exit 7"}
		cases["silent command"] = PasswordSources{Command: "true"}
	}
	for name, src := range cases {
		_, err := resolvePasswordWith("", src, 2*time.Second)
		if !isNotConfigured(err) {
			t.Errorf("%s: %v", name, err)
		}
	}
}

// A set source that fails must not fall through to the next one.
func TestPasswordFailingFileDoesNotFallThroughToCommand(t *testing.T) {
	if runtime.GOOS == "windows" {
		t.Skip("uses sh")
	}
	_, err := resolvePasswordWith("", PasswordSources{
		File: filepath.Join(t.TempDir(), "absent"), Command: "echo fallback",
	}, 2*time.Second)
	if !isNotConfigured(err) {
		t.Fatalf("got %v", err)
	}
}

func TestPasswordCommandTimeoutKillsIt(t *testing.T) {
	if runtime.GOOS == "windows" {
		t.Skip("uses sh")
	}
	start := time.Now()
	_, err := resolvePasswordWith("", PasswordSources{Command: "sleep 30"}, 200*time.Millisecond)
	if !isNotConfigured(err) || !strings.Contains(err.Error(), "did not finish") {
		t.Fatalf("got %v", err)
	}
	if time.Since(start) > 5*time.Second {
		t.Fatal("the timeout was not enforced")
	}
}

func TestMissingPasswordMessageNamesAllThreeSources(t *testing.T) {
	_, err := resolvePasswordWith("", PasswordSources{}, time.Second)
	for _, want := range []string{"JETKVM_PASSWORD", "--password-file", "--password-command"} {
		if err == nil || !strings.Contains(err.Error(), want) {
			t.Fatalf("%q missing from %v", want, err)
		}
	}
}
