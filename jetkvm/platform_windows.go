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

//go:build windows

package main

import "os"

// Windows has no uid; the runtime namespace uses the user name instead. The
// directory sits inside the user's profile, whose ACL already excludes other
// non-admin users, so "exists and is a directory" is the whole check.
func currentUID() int { return 0 }

func ensurePrivateDir(dir string) error { return os.MkdirAll(dir, 0o700) }

func pidAlive(pid int) bool {
	if pid <= 0 {
		return false
	}
	_, err := os.FindProcess(pid)
	return err == nil
}

// lockFile is best-effort on Windows: opening the file for write already
// excludes a second daemon only if it fails to open, so this is a no-op.
func lockFile(*os.File) error { return nil }
