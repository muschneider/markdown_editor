# Build and install md_editor on Linux.
#
#   make                  # release build
#   make install          # install into ~/.local (no sudo)
#   make install PREFIX=/usr/local
#   make uninstall
#
# PREFIX defaults to ~/.local so a plain `make install` never needs root.
# DESTDIR is honoured for staged installs (see packaging/PKGBUILD), in which
# case the desktop/icon cache refresh is skipped — the package manager owns it.

PREFIX  ?= $(HOME)/.local
DESTDIR ?=
CARGO   ?= cargo
INSTALL ?= install

BIN     := md_editor
VERSION := $(shell sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n1)
TRIPLE  := $(shell rustc -vV | sed -n 's/^host: //p')

RELEASE_BIN := target/release/$(BIN)

bindir  := $(DESTDIR)$(PREFIX)/bin
datadir := $(DESTDIR)$(PREFIX)/share
appsdir := $(datadir)/applications
icondir := $(datadir)/icons/hicolor/scalable/apps

DIST_NAME := $(BIN)-$(VERSION)-$(TRIPLE)
DIST_DIR  := dist/$(DIST_NAME)

.PHONY: all build run test bench fmt fmt-check lint check \
        install install-files uninstall update-caches dist clean help

all: build

## build: compile the release binary
build:
	$(CARGO) build --release

## run: build and run in debug mode
run:
	$(CARGO) run

## test: run the test suite
test:
	$(CARGO) test

## bench: run the ignored performance benchmarks
bench:
	$(CARGO) test --release -- --ignored --nocapture

## fmt: format the source
fmt:
	$(CARGO) fmt

## fmt-check: fail if the source is unformatted
fmt-check:
	$(CARGO) fmt -- --check

## lint: run clippy with warnings denied
lint:
	$(CARGO) clippy -- -D warnings

## check: everything CI would run
check: fmt-check lint test

## install: build, then install the binary, desktop entry and icon
install: build install-files update-caches

## install-files: install without building (for packagers and binary tarballs)
install-files:
	$(INSTALL) -Dm755 $(RELEASE_BIN)             $(bindir)/$(BIN)
	$(INSTALL) -Dm644 packaging/$(BIN).desktop   $(appsdir)/$(BIN).desktop
	$(INSTALL) -Dm644 assets/$(BIN).svg          $(icondir)/$(BIN).svg
	@echo
	@echo "Installed to $(DESTDIR)$(PREFIX)."
	@case ":$$PATH:" in \
	  *":$(PREFIX)/bin:"*) ;; \
	  *) echo "Note: $(PREFIX)/bin is not on your PATH." ;; \
	esac

## uninstall: remove everything install placed
uninstall:
	$(RM) $(bindir)/$(BIN)
	$(RM) $(appsdir)/$(BIN).desktop
	$(RM) $(icondir)/$(BIN).svg
	@$(MAKE) --no-print-directory update-caches

# Desktop environments cache .desktop entries and icons; without this the
# launcher entry only appears after the next login. Both tools are optional,
# and both are meaningless against a staging root.
ifeq ($(strip $(DESTDIR)),)
update-caches:
	@command -v update-desktop-database >/dev/null 2>&1 \
	    && update-desktop-database -q "$(appsdir)" || true
	@command -v gtk-update-icon-cache >/dev/null 2>&1 \
	    && gtk-update-icon-cache -qtf "$(datadir)/icons/hicolor" || true
else
update-caches:
	@:
endif

## dist: build a relocatable binary tarball in dist/
dist: build
	$(RM) -r $(DIST_DIR)
	mkdir -p $(DIST_DIR)
	$(INSTALL) -m755 $(RELEASE_BIN)           $(DIST_DIR)/$(BIN)
	$(INSTALL) -m755 packaging/install.sh     $(DIST_DIR)/install.sh
	$(INSTALL) -m644 packaging/$(BIN).desktop $(DIST_DIR)/$(BIN).desktop
	$(INSTALL) -m644 assets/$(BIN).svg        $(DIST_DIR)/$(BIN).svg
	$(INSTALL) -m644 README.md                $(DIST_DIR)/README.md
	@[ -f LICENSE ] && $(INSTALL) -m644 LICENSE $(DIST_DIR)/LICENSE || true
	tar -C dist -czf dist/$(DIST_NAME).tar.gz $(DIST_NAME)
	@echo "dist/$(DIST_NAME).tar.gz"

## clean: remove build and dist artifacts
clean:
	$(CARGO) clean
	$(RM) -r dist

## help: list targets
help:
	@sed -n 's/^## //p' $(MAKEFILE_LIST) | awk -F': ' '{printf "  %-14s %s\n", $$1, $$2}'
