CARGO_HOME ?= $(CURDIR)/.cargo
RUSTUP_HOME ?= $(CURDIR)/.rustup
export CARGO_HOME RUSTUP_HOME
PATH := $(CARGO_HOME)/bin:$(PATH)
CARGO := $(CARGO_HOME)/bin/cargo
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

.PHONY: build run test lint fmt install install-service clean

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

install: build
	install -Dm755 $(RELEASE_DIR)/z13helper $(BINDIR)/z13helper
	install -Dm755 $(RELEASE_DIR)/z13helperctl $(BINDIR)/z13helperctl
	install -Dm644 contrib/com.ashtonantila.z13helper.desktop $(DESKTOPDIR)/com.ashtonantila.z13helper.desktop
	install -Dm644 assets/z13helper.svg $(ICONDIR)/z13helper.svg

# Privileged machine backend. Invoke this target as root.
install-service: build
	install -Dm755 $(RELEASE_DIR)/z13helperd $(LIBEXECDIR)/z13helperd
	install -Dm644 contrib/systemd/z13helperd.service $(SYSTEMDUNITDIR)/z13helperd.service
	install -Dm644 contrib/sysusers.d/z13helper.conf $(SYSUSERSDIR)/z13helper.conf
	systemd-sysusers z13helper.conf
	systemctl daemon-reload
	systemctl reset-failed z13helperd.service || true
	systemctl enable --now z13helperd.service
	systemctl restart z13helperd.service
	@echo "Add your user to the z13helper group, then re-login:"
	@echo "  sudo usermod -aG z13helper \$$USER"

clean:
	$(CARGO) clean
