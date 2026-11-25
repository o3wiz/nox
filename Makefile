.PHONY: help fmt fmt-check lint test doc check release macos-app package-macos install-macos package-linux clean-dist

VERSION := $(shell sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n 1)
HOST_ARCH := $(shell uname -m)
DIST_DIR := dist
CARGO_TARGET_DIR ?= target
BIN_NAME := nox

ifeq ($(HOST_ARCH),arm64)
	MAC_ARCH := arm64
	MAC_TARGET := aarch64-apple-darwin
else
	MAC_ARCH := x86_64
	MAC_TARGET := x86_64-apple-darwin
endif

LINUX_TARGET := $(HOST_ARCH)-unknown-linux-gnu

help:
	@echo "Nox $(VERSION) — build, quality, and packaging targets"
	@echo ""
	@echo "Development & verification:"
	@echo "  make fmt            Format Rust sources with rustfmt"
	@echo "  make fmt-check      Verify formatting without modifying files"
	@echo "  make lint           Run clippy with warnings denied"
	@echo "  make test           Run unit, UI, and CLI integration tests"
	@echo "  make doc            Build documentation with warnings denied"
	@echo "  make check          Run full CI check suite (fmt, lint, test, doc, config)"
	@echo ""
	@echo "Delivery & packaging:"
	@echo "  make release        Build optimized release binary ($(CARGO_TARGET_DIR)/release/$(BIN_NAME))"
	@echo "  make macos-app      Build signed macOS app bundle ($(DIST_DIR)/Nox.app)"
	@echo "  make package-macos  Build macOS .app, .dmg, .app.zip, CLI .tar.gz, and SHA256SUMS"
	@echo "  make install-macos  Build and install Nox.app into /Applications"
	@echo "  make package-linux  Build Linux release .tar.gz with desktop entry, icon, and SHA256SUMS"
	@echo "  make clean-dist     Remove $(DIST_DIR)/ packaging output"

fmt:
	cargo fmt --all

fmt-check:
	cargo fmt --all -- --check

lint:
	cargo clippy --all-targets --locked -- -D warnings

test:
	cargo test --locked

doc:
	RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --locked

check: fmt-check lint test doc
	cargo run --locked -- --version
	cargo run --locked -- --check-config

release:
	cargo build --release --locked

macos-app: release
	@rm -rf "$(DIST_DIR)/Nox.app"
	@mkdir -p "$(DIST_DIR)/Nox.app/Contents/MacOS" "$(DIST_DIR)/Nox.app/Contents/Resources"
	@cp "$(CARGO_TARGET_DIR)/release/$(BIN_NAME)" "$(DIST_DIR)/Nox.app/Contents/MacOS/$(BIN_NAME)"
	@chmod +x "$(DIST_DIR)/Nox.app/Contents/MacOS/$(BIN_NAME)"
	@cp packaging/macos/Nox.icns "$(DIST_DIR)/Nox.app/Contents/Resources/Nox.icns"
	@sed 's/@VERSION@/$(VERSION)/g' packaging/macos/Info.plist > "$(DIST_DIR)/Nox.app/Contents/Info.plist"
	@plutil -lint "$(DIST_DIR)/Nox.app/Contents/Info.plist"
	@codesign --force --deep --sign - "$(DIST_DIR)/Nox.app"
	@echo "Built $(DIST_DIR)/Nox.app (v$(VERSION))"

package-macos: macos-app
	@rm -rf "$(DIST_DIR)/dmg-stage" "$(DIST_DIR)/cli-stage"
	@rm -f "$(DIST_DIR)/Nox-$(VERSION)-$(MAC_ARCH).dmg" \
		"$(DIST_DIR)/Nox-$(VERSION)-$(MAC_ARCH).app.zip" \
		"$(DIST_DIR)/nox-v$(VERSION)-$(MAC_TARGET).tar.gz"
	@mkdir -p "$(DIST_DIR)/dmg-stage"
	@ditto "$(DIST_DIR)/Nox.app" "$(DIST_DIR)/dmg-stage/Nox.app"
	@ln -s /Applications "$(DIST_DIR)/dmg-stage/Applications"
	@hdiutil create -volname "Nox $(VERSION)" \
		-srcfolder "$(DIST_DIR)/dmg-stage" \
		-ov -format UDZO \
		"$(DIST_DIR)/Nox-$(VERSION)-$(MAC_ARCH).dmg"
	@rm -rf "$(DIST_DIR)/dmg-stage"
	@ditto -c -k --sequesterRsrc --keepParent \
		"$(DIST_DIR)/Nox.app" \
		"$(DIST_DIR)/Nox-$(VERSION)-$(MAC_ARCH).app.zip"
	@mkdir -p "$(DIST_DIR)/cli-stage/nox-v$(VERSION)-$(MAC_TARGET)"
	@cp "$(CARGO_TARGET_DIR)/release/$(BIN_NAME)" "$(DIST_DIR)/cli-stage/nox-v$(VERSION)-$(MAC_TARGET)/$(BIN_NAME)"
	@cp README.md "$(DIST_DIR)/cli-stage/nox-v$(VERSION)-$(MAC_TARGET)/"
	@cp -R config schemas "$(DIST_DIR)/cli-stage/nox-v$(VERSION)-$(MAC_TARGET)/"
	@tar -C "$(DIST_DIR)/cli-stage" -czf "$(DIST_DIR)/nox-v$(VERSION)-$(MAC_TARGET).tar.gz" "nox-v$(VERSION)-$(MAC_TARGET)"
	@rm -rf "$(DIST_DIR)/cli-stage"
	@(cd "$(DIST_DIR)" && shasum -a 256 \
		"Nox-$(VERSION)-$(MAC_ARCH).dmg" \
		"Nox-$(VERSION)-$(MAC_ARCH).app.zip" \
		"nox-v$(VERSION)-$(MAC_TARGET).tar.gz" > SHA256SUMS.txt)
	@echo "Packaged macOS release artifacts in $(DIST_DIR)/:"
	@ls -lh "$(DIST_DIR)"

install-macos: macos-app
	@rm -rf "/Applications/Nox.app"
	@ditto "$(DIST_DIR)/Nox.app" "/Applications/Nox.app"
	@echo "Installed /Applications/Nox.app (v$(VERSION))"

package-linux: release
	@rm -rf "$(DIST_DIR)/linux-stage"
	@mkdir -p "$(DIST_DIR)/linux-stage/nox-v$(VERSION)-$(LINUX_TARGET)"
	@cp "$(CARGO_TARGET_DIR)/release/$(BIN_NAME)" "$(DIST_DIR)/linux-stage/nox-v$(VERSION)-$(LINUX_TARGET)/$(BIN_NAME)"
	@cp packaging/linux/nox.desktop packaging/assets/nox.svg README.md \
		"$(DIST_DIR)/linux-stage/nox-v$(VERSION)-$(LINUX_TARGET)/"
	@cp -R config schemas "$(DIST_DIR)/linux-stage/nox-v$(VERSION)-$(LINUX_TARGET)/"
	@tar -C "$(DIST_DIR)/linux-stage" -czf "$(DIST_DIR)/nox-v$(VERSION)-$(LINUX_TARGET).tar.gz" "nox-v$(VERSION)-$(LINUX_TARGET)"
	@rm -rf "$(DIST_DIR)/linux-stage"
	@(cd "$(DIST_DIR)" && sha256sum "nox-v$(VERSION)-$(LINUX_TARGET).tar.gz" > SHA256SUMS.txt)
	@echo "Packaged Linux release artifact in $(DIST_DIR)/"

clean-dist:
	rm -rf "$(DIST_DIR)"
