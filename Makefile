.PHONY: release clean

CURRENT_VERSION := $(shell grep '^version' Cargo.toml | head -1 | cut -d'"' -f2)

release:
	@if [ -z "$$CODESIGN_IDENTITY" ]; then \
		echo "error: CODESIGN_IDENTITY not set - release builds require signing"; \
		echo "hint: plug in YubiKey and enter the devShell (direnv allow)"; \
		exit 1; \
	fi
	@printf "current version: $(CURRENT_VERSION)\nnew version: "; \
	read VERSION; \
	if [ -z "$$VERSION" ]; then echo "error: no version provided"; exit 1; fi; \
	sed -i"" "s|^version = \".*\"|version = \"$$VERSION\"|" Cargo.toml; \
	sed -i"" "s|version = \".*\";|version = \"$$VERSION\";|" flake.nix; \
	/usr/bin/plutil -replace CFBundleVersion -string "$$VERSION" Info.plist; \
	/usr/bin/plutil -replace CFBundleShortVersionString -string "$$VERSION" Info.plist; \
	cargo build --release; \
	rm -rf dist; \
	mkdir -p dist/psst.app/Contents/MacOS dist/psst.app/Contents/Resources; \
	cp target/release/psst dist/psst.app/Contents/MacOS/psst; \
	cp Info.plist dist/psst.app/Contents/Info.plist; \
	cp -R psst.icon dist/psst.app/Contents/Resources/psst.icon; \
	codesign --force --sign "$$CODESIGN_IDENTITY" dist/psst.app; \
	TARBALL="psst-$$VERSION-aarch64-darwin.tar.gz"; \
	tar -czf "$$TARBALL" -C dist psst.app; \
	HASH=$$(nix hash file --type sha256 --sri "$$TARBALL"); \
	sed -i"" "s|darwinHash = \".*\"|darwinHash = \"$$HASH\"|" flake.nix; \
	echo "built $$TARBALL"; \
	echo "hash: $$HASH (updated in flake.nix)"; \
	echo "next: commit, gh release create v$$VERSION $$TARBALL, push"

clean:
	cargo clean
	rm -rf dist *.tar.gz
