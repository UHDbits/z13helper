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

.PHONY: build run test lint fmt install install-fan-service clean

build:
	$(CARGO) build --release -p z13-helper -p z13-helper-fan-service

run:
	$(CARGO) run -p z13-helper

test:
	$(CARGO) test --workspace

lint:
	$(CARGO) clippy --workspace --all-targets -- -D warnings
	$(CARGO) fmt --all -- --check

fmt:
	$(CARGO) fmt --all

install: build
	install -Dm755 $(RELEASE_DIR)/z13-helper $(BINDIR)/z13-helper
	install -Dm644 contrib/z13-helper.desktop $(DESKTOPDIR)/z13-helper.desktop
	install -Dm644 assets/z13-helper.svg $(ICONDIR)/z13-helper.svg

# Privileged companion for experimental direct EC fan control. Requires root.
install-fan-service: build
	install -Dm755 $(RELEASE_DIR)/z13-helper-fan-service $(LIBEXECDIR)/z13-helper-fan-service
	install -Dm644 contrib/systemd/z13-helper-fan-service.service $(SYSTEMDUNITDIR)/z13-helper-fan-service.service
	install -Dm644 contrib/sysusers.d/z13-helper.conf $(SYSUSERSDIR)/z13-helper.conf
	systemd-sysusers z13-helper.conf
	systemctl daemon-reload
	systemctl reset-failed z13-helper-fan-service.service || true
	systemctl enable --now z13-helper-fan-service.service
	systemctl restart z13-helper-fan-service.service
	@echo "Add your user to the z13-helper group, then re-login:"
	@echo "  sudo usermod -aG z13-helper \$$USER"

clean:
	$(CARGO) clean
