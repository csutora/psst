.PHONY: release clean

release:
	@if [ -z "$$CODESIGN_IDENTITY" ]; then \
		echo "error: CODESIGN_IDENTITY not set - release builds require signing"; \
		echo "hint: plug in YubiKey and enter the devShell (direnv allow)"; \
		exit 1; \
	fi
	cargo build --release
	rm -rf dist
	mkdir -p dist/psst.app/Contents/MacOS
	cp target/release/psst dist/psst.app/Contents/MacOS/psst
	cp Info.plist dist/psst.app/Contents/Info.plist
	codesign --force --sign "$$CODESIGN_IDENTITY" dist/psst.app
	@VERSION=$$(grep '^version' Cargo.toml | head -1 | cut -d'"' -f2); \
	TARBALL="psst-$$VERSION-aarch64-darwin.tar.gz"; \
	tar -czf "$$TARBALL" -C dist psst.app; \
	HASH=$$(nix hash file --type sha256 --sri "$$TARBALL"); \
	sed -i"" "s|darwinHash = \".*\"|darwinHash = \"$$HASH\"|" flake.nix; \
	echo "built $$TARBALL"; \
	echo "hash: $$HASH (updated in flake.nix)"

clean:
	cargo clean
	rm -rf dist *.tar.gz
