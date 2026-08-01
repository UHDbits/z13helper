CARGO_HOME ?= $(CURDIR)/.cargo
RUSTUP_HOME ?= $(CURDIR)/.rustup
export CARGO_HOME RUSTUP_HOME
PATH := $(CARGO_HOME)/bin:$(PATH)
CARGO := $(CARGO_HOME)/bin/cargo

PREFIX ?= $(HOME)/.local
BINDIR := $(PREFIX)/bin
DATADIR := $(PREFIX)/share
DESKTOPDIR := $(DATADIR)/applications
ICONDIR := $(DATADIR)/icons/hicolor/scalable/apps

.PHONY: build run test lint fmt install clean

build:
	$(CARGO) build --release

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
	install -Dm755 target/release/z13-helper $(BINDIR)/z13-helper
	install -Dm644 contrib/z13-helper.desktop $(DESKTOPDIR)/z13-helper.desktop
	install -Dm644 assets/z13-helper.svg $(ICONDIR)/z13-helper.svg

clean:
	$(CARGO) clean
