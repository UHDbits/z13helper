CARGO ?= cargo
# Honor an external CARGO_TARGET_DIR (Cursor sandboxes set this) so install
# picks up the binary that `make build` actually produced.
TARGET_DIR := $(if $(CARGO_TARGET_DIR),$(CARGO_TARGET_DIR),$(CURDIR)/target)
RELEASE_DIR := $(TARGET_DIR)/release

PREFIX ?= $(HOME)/.local
BINDIR := $(PREFIX)/bin
DATADIR := $(PREFIX)/share
DESKTOPDIR := $(DATADIR)/applications
ICONDIR := $(DATADIR)/icons/hicolor/scalable/apps
LIBEXECDIR ?= /usr/libexec
SYSTEMDUNITDIR ?= /usr/lib/systemd/system
SYSUSERSDIR ?= /usr/lib/sysusers.d

BPF_SOURCE := crates/z13helperd/bpf/hidraw_blocker.bpf.c
BPF_OBJECT := crates/z13helperd/bpf/hidraw_blocker.bpf.o
BPF_CHECK_OBJECT := $(TARGET_DIR)/bpf-check/hidraw_blocker.bpf.o
BPF_CLANG ?= clang
BPF_STRIP ?= llvm-strip
BPF_CFLAGS ?= -O2 -g -target bpf -D__TARGET_ARCH_x86
BPF_INCLUDE_FLAGS ?= -I/usr/include -I/usr/include/x86_64-linux-gnu
# BTF.ext line information records the compilation directory. Use a stable,
# synthetic path so the tracked object is checkout-independent and does not
# disclose a developer's local filesystem.
BPF_DEBUG_COMPILATION_DIR ?= /build/z13helper

.PHONY: build run test lint fmt check-bpf bpf install install-user-service install-service clean

build:
	$(CARGO) build --release -p z13helper -p z13helperd -p z13helperctl

run:
	$(CARGO) run -p z13helper

test:
	$(CARGO) test --workspace

lint:
	$(CARGO) clippy --workspace --all-targets -- -D warnings
	$(CARGO) fmt --all -- --check

fmt:
	$(CARGO) fmt --all

check-bpf:
	mkdir -p $(TARGET_DIR)/bpf-check
	$(BPF_CLANG) $(BPF_CFLAGS) $(BPF_INCLUDE_FLAGS) -fdebug-compilation-dir="$(BPF_DEBUG_COMPILATION_DIR)" -c $(BPF_SOURCE) -o $(BPF_CHECK_OBJECT)
	$(BPF_STRIP) --strip-debug $(BPF_CHECK_OBJECT)
	cmp $(BPF_OBJECT) $(BPF_CHECK_OBJECT)

bpf:
	$(BPF_CLANG) $(BPF_CFLAGS) $(BPF_INCLUDE_FLAGS) -fdebug-compilation-dir="$(BPF_DEBUG_COMPILATION_DIR)" -c $(BPF_SOURCE) -o $(BPF_OBJECT)
	$(BPF_STRIP) --strip-debug $(BPF_OBJECT)

install: build
	install -Dm755 $(RELEASE_DIR)/z13helper $(BINDIR)/z13helper
	install -Dm755 $(RELEASE_DIR)/z13helperctl $(BINDIR)/z13helperctl
	install -Dm644 contrib/com.ashtonantila.z13helper.desktop $(DESKTOPDIR)/com.ashtonantila.z13helper.desktop
	install -Dm644 assets/z13helper.svg $(ICONDIR)/z13helper.svg

# Per-user GUI service. This follows graphical-session.target so it also works
# in SteamOS-style sessions where desktop autostart entries are not used.
install-user-service: install
	sed 's|%h/.local/bin|$(BINDIR)|' contrib/z13helper.service | install -Dm644 /dev/stdin $(HOME)/.config/systemd/user/z13helper.service
	systemctl --user daemon-reload
	systemctl --user enable --now z13helper.service

# Privileged machine backend. Invoke this target as root.
install-service: build
	install -Dm755 $(RELEASE_DIR)/z13helperd $(LIBEXECDIR)/z13helperd
	sed 's|/usr/libexec|$(LIBEXECDIR)|g' contrib/systemd/z13helperd.service | install -Dm644 /dev/stdin $(SYSTEMDUNITDIR)/z13helperd.service
	install -Dm644 contrib/sysusers.d/z13helper.conf $(SYSUSERSDIR)/z13helper.conf
	systemd-sysusers z13helper.conf
	systemctl daemon-reload
	systemctl reset-failed z13helperd.service || true
	systemctl enable z13helperd.service
	if systemctl is-active --quiet z13helperd.service; then systemctl restart z13helperd.service; else systemctl start z13helperd.service; fi
	@echo "Add your user to the z13helper group, then re-login:"
	@echo "  sudo usermod -aG z13helper \$$USER"

clean:
	$(CARGO) clean
