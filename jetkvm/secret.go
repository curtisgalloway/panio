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
	"bytes"
	"fmt"
	"os"
	"os/exec"
	"runtime"
	"strings"
	"time"
)

// Password resolution mirrors the Rust `secret` crate: environment variable
// first (an empty one counts as unset), then --password-file, then
// --password-command. The first source that is set wins and a failure there
// does not fall through. The password is never taken from a flag value.

const (
	passwordEnv        = "JETKVM_PASSWORD"
	passwordFileFlag   = "--password-file"
	passwordCmdFlag    = "--password-command"
	passwordCmdTimeout = 30 * time.Second
)

// NotConfigured means no usable password. main maps it to exit status 3.
type NotConfigured struct{ Msg string }

func (e *NotConfigured) Error() string { return e.Msg }

// PasswordSources are the places a password may come from.
type PasswordSources struct {
	File    string
	Command string
}

func passwordHint() string {
	return fmt.Sprintf("set %s, or pass %s <path> or %s <cmd>", passwordEnv, passwordFileFlag, passwordCmdFlag)
}

// resolvePassword is resolvePasswordWith using the real environment and the
// 30 s command timeout.
func resolvePassword(src PasswordSources) (string, error) {
	return resolvePasswordWith(os.Getenv(passwordEnv), src, passwordCmdTimeout)
}

// resolvePasswordWith takes the environment value and timeout as parameters so
// tests need neither the process environment nor a 30 s wait.
func resolvePasswordWith(env string, src PasswordSources, timeout time.Duration) (string, error) {
	if env != "" {
		return env, nil
	}
	if src.File != "" {
		return readPasswordFile(src.File)
	}
	if src.Command != "" {
		return runPasswordCommand(src.Command, timeout)
	}
	return "", &NotConfigured{"the JetKVM password is not set -- " + passwordHint() +
		". It is never taken from a flag's value or a config file, so it cannot " +
		"leak into a lab file, shell history, or `ps`"}
}

func trimOneNewline(s string) string {
	if strings.HasSuffix(s, "\n") {
		s = s[:len(s)-1]
		s = strings.TrimSuffix(s, "\r")
	}
	return s
}

func readPasswordFile(path string) (string, error) {
	b, err := os.ReadFile(path)
	if err != nil {
		return "", &NotConfigured{fmt.Sprintf("%s: cannot read password file %s: %v", passwordFileFlag, path, err)}
	}
	warnIfExposed(path)
	v := trimOneNewline(string(b))
	if v == "" {
		return "", &NotConfigured{fmt.Sprintf("%s: %s is empty", passwordFileFlag, path)}
	}
	return v, nil
}

// warnIfExposed warns, without refusing, when the file is readable by group or
// others (container secret mounts are commonly 0444).
func warnIfExposed(path string) {
	if runtime.GOOS == "windows" {
		return
	}
	if fi, err := os.Stat(path); err == nil {
		if m := fi.Mode().Perm(); m&0o044 != 0 {
			fmt.Fprintf(os.Stderr, "warning: password file %s is readable by group or others (mode %03o); consider chmod 600\n", path, m)
		}
	}
}

func runPasswordCommand(cmd string, timeout time.Duration) (string, error) {
	fail := func(why string) error {
		return &NotConfigured{fmt.Sprintf("%s `%s` %s", passwordCmdFlag, cmd, why)}
	}
	var c *exec.Cmd
	if runtime.GOOS == "windows" {
		c = exec.Command("cmd", "/C", cmd)
	} else {
		c = exec.Command("sh", "-c", cmd)
	}
	// No stdin: a command that prompts must fail, not hang. Its stderr passes
	// through; its stdout is the secret and is never logged.
	var out bytes.Buffer
	c.Stdout = &out
	c.Stderr = os.Stderr
	c.Stdin = nil
	if err := c.Start(); err != nil {
		return "", fail("could not be started: " + err.Error())
	}
	done := make(chan error, 1)
	go func() { done <- c.Wait() }()
	select {
	case err := <-done:
		if err != nil {
			return "", fail("failed: " + err.Error())
		}
	case <-time.After(timeout):
		_ = c.Process.Kill()
		return "", fail(fmt.Sprintf("did not finish within %v and was killed", timeout))
	}
	v := trimOneNewline(out.String())
	if v == "" {
		return "", fail("printed nothing")
	}
	return v, nil
}
