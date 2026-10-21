# Copyright 2026 Curtis Galloway
#
# Licensed under the Apache License, Version 2.0 (the "License");
# you may not use this file except in compliance with the License.
# You may obtain a copy of the License at
#
#     http://www.apache.org/licenses/LICENSE-2.0
#
# Unless required by applicable law or agreed to in writing, software
# distributed under the License is distributed on an "AS IS" BASIS,
# WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
# See the License for the specific language governing permissions and
# limitations under the License.

# Build and install panio: the Rust CLI (cli/) plus the daemons and helpers
# (hdmicap, serialcap, netbootd, cambrionix, hidrig, ch9329, shellyplug,
# switchport, amt), the OCR helper, and the zigplug Zigbee helper (Python, installed by
# `panio setup` as a uv tool).
# Only the `panio` CLI lands on PATH (~/.cargo/bin); the helpers install
# into the private libexec dir (~/.local/libexec/panio/bin), run via
# `panio helper <name> ...` when needed directly.
# `make install` from a fresh clone is the only command you need; re-run it
# after editing anything to rebuild and reinstall.

CRATES = cli hdmicap serialcap netbootd cambrionix hidrig ch9329 shellyplug switchport amt secret

# The installed CLI, by absolute path: immune to a different `panio` shadowing
# ~/.cargo/bin earlier in PATH (e.g. a Homebrew keg from the tap).
PANIO ?= $(HOME)/.cargo/bin/panio

.PHONY: help install reinstall rust test fmt clean check-shadow check-deps

help:
	@echo "panio build targets:"
	@echo "  make install    Build + install everything: the panio CLI, the daemons,"
	@echo "                   and platform steps via 'panio setup' (OCR helper,"
	@echo "                   bpf-helper setuid on macOS, group setup on Linux)."
	@echo "  make reinstall   Alias for 'make install' (install is already a full rebuild)."
	@echo "  make rust        Fast path: build + install the Rust crates ($(CRATES)) only,"
	@echo "                   skipping the OCR/setuid/zigplug/group steps."
	@echo "  make test        cargo test every crate."
	@echo "  make fmt         rustfmt every crate."
	@echo "  make clean       cargo clean every crate."

# Full build + install. Bootstrap the CLI with cargo, then let `panio setup`
# do the rest — it rebuilds all crates and applies the platform-specific steps,
# so all that logic lives in one place (cli/src/setup.rs).
install: check-shadow check-deps
	cargo install --path cli
	$(PANIO) setup

reinstall: install

# Fail before the first cargo invocation when a Linux build prerequisite is
# missing, with an install hint — instead of a cryptic build error minutes in.
# pkg-config + libudev-dev: serial-port enumeration (libudev-sys).
# cmake + nasm: hdmicap's turbojpeg dep builds a vendored libjpeg-turbo
#   (Debian's system libturbojpeg is too old for the crate), and the crate's
#   require-simd default makes nasm mandatory on x86-64.
# libclang-dev: V4L2 bindgen (v4l2-sys-mit) in hdmicap.
check-deps:
	@if [ "$$(uname -s)" = "Linux" ]; then \
		fail=0; missing=""; \
		if ! command -v cargo >/dev/null 2>&1; then \
			echo "ERROR: cargo not found — install Rust (https://rustup.rs)"; \
			fail=1; \
		fi; \
		command -v pkg-config >/dev/null 2>&1 || missing="$$missing pkg-config"; \
		command -v cmake >/dev/null 2>&1 || missing="$$missing cmake"; \
		command -v nasm >/dev/null 2>&1 || missing="$$missing nasm"; \
		if command -v pkg-config >/dev/null 2>&1; then \
			pkg-config --exists libudev || missing="$$missing libudev-dev"; \
		elif [ ! -e /usr/include/libudev.h ]; then \
			missing="$$missing libudev-dev"; \
		fi; \
		set -- /usr/lib/llvm-*/lib/libclang.so; \
		[ -e "$$1" ] || missing="$$missing libclang-dev"; \
		if [ -n "$$missing" ]; then \
			echo "ERROR: missing system packages:$$missing"; \
			echo "       install with: sudo apt-get install$$missing"; \
			fail=1; \
		fi; \
		[ "$$fail" = "0" ] || exit 1; \
	fi

# Warn when a different `panio` shadows the freshly installed one — e.g. a
# Homebrew keg from the tap winning over ~/.cargo/bin earlier in PATH.
check-shadow:
	@found=$$(command -v panio 2>/dev/null); \
	if [ -n "$$found" ] && [ "$$found" != "$(PANIO)" ]; then \
		echo "WARNING: 'panio' in PATH is $$found, not $(PANIO)."; \
		echo "         Ensure ~/.cargo/bin precedes it in PATH, or remove the"; \
		echo "         shadowing copy (e.g. 'brew uninstall panio')."; \
	fi

# Fast path for iterating on the Rust code without re-running the full setup.
# `--rust-only` does what needs nothing beyond cargo -- the crate builds, the
# bundled skills, the stale-copy cleanup -- and skips what needs sudo or a
# second toolchain (OCR, zigplug, the macOS setuid step, the Linux device-group
# check); re-run `make install` when you need those. Bootstraps the CLI first so
# install-layout changes in setup.rs take effect, then lets
# `panio setup --rust-only` place the helpers and skills -- the
# libexec-vs-PATH layout logic lives in one place (cli/src/setup.rs).
rust: check-deps
	cargo install --path cli
	$(PANIO) setup --rust-only

test:
	@bash scripts/ci-coverage-check.sh
	@for crate in $(CRATES); do \
		echo "==> cargo test ($$crate)"; \
		( cd $$crate && cargo test ) || exit 1; \
	done

fmt:
	@for crate in $(CRATES); do \
		( cd $$crate && cargo fmt ); \
	done

clean:
	@for crate in $(CRATES); do \
		( cd $$crate && cargo clean ); \
	done
