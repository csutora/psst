{
  description = "psst - a matrix notification daemon";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
  };

  outputs = { self, nixpkgs }:
    let
      darwinSystems = [ "aarch64-darwin" ];
      linuxSystems = [ "x86_64-linux" "aarch64-linux" ];
      allSystems = darwinSystems ++ linuxSystems;

      forDarwin = f: nixpkgs.lib.genAttrs darwinSystems (system: f nixpkgs.legacyPackages.${system});
      forLinux = f: nixpkgs.lib.genAttrs linuxSystems (system: f nixpkgs.legacyPackages.${system});
      forAll = f: nixpkgs.lib.genAttrs allSystems (system: f nixpkgs.legacyPackages.${system});

      version = "0.1.7";

      # darwin: fetch pre-built signed binary from github releases
      # update hash after `make dist && gh release create`
      darwinUrl = "https://github.com/csutora/psst/releases/download/v${version}/psst-${version}-aarch64-darwin.tar.gz";
      darwinHash = "sha256-YuvC7hIxZrYJOV+FeqwhsGWglFBGIW7xnj51D9lnL6U=";

      mkDarwin = pkgs: pkgs.stdenv.mkDerivation {
        pname = "psst";
        inherit version;
        src = pkgs.fetchurl {
          url = darwinUrl;
          hash = darwinHash;
        };

        sourceRoot = ".";
        dontBuild = true;
        dontConfigure = true;
        dontFixup = true;

        unpackPhase = ''
          tar xzf $src
        '';

        installPhase = ''
          mkdir -p $out/Applications $out/bin
          cp -R psst.app $out/Applications/
          ln -s $out/Applications/psst.app/Contents/MacOS/psst $out/bin/psst
        '';

        meta = {
          description = "matrix notification daemon";
          platforms = darwinSystems;
          mainProgram = "psst";
        };
      };

      # linux: build from source
      mkLinux = pkgs: pkgs.rustPlatform.buildRustPackage {
        pname = "psst";
        inherit version;
        src = ./.;

        cargoLock.lockFile = ./Cargo.lock;

        nativeBuildInputs = [ pkgs.pkg-config ];
        buildInputs = [ pkgs.sqlite pkgs.dbus pkgs.openssl ];

        meta = {
          description = "matrix notification daemon";
          platforms = linuxSystems;
          mainProgram = "psst";
        };
      };
    in
    {
      packages =
        (forDarwin (pkgs: rec {
          psst = mkDarwin pkgs;
          default = psst;
        }))
        // (forLinux (pkgs: rec {
          psst = mkLinux pkgs;
          default = psst;
        }));

      devShells = forAll (pkgs: {
        default = pkgs.mkShell {
          inputsFrom = builtins.attrValues (
            nixpkgs.lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux {
              inherit (self.packages.${pkgs.stdenv.hostPlatform.system}) psst;
            }
          );

          packages = with pkgs; [
            rust-analyzer
            cargo-watch
          ] ++ lib.optionals stdenv.hostPlatform.isLinux [
            rustc
            cargo
            clippy
            rustfmt
          ];

          buildInputs = pkgs.lib.optionals pkgs.stdenv.hostPlatform.isDarwin [
            pkgs.apple-sdk_14
            pkgs.libiconv
          ];

          shellHook = ''
            IDENTITY=$(security find-identity -v -p codesigning 2>/dev/null | awk '/Apple Development/ {print $2; exit}')
            if [ -n "$IDENTITY" ]; then
              export CODESIGN_IDENTITY="$IDENTITY"
            fi
          '';
        };
      });

      overlays.default = final: prev: {
        psst = self.packages.${prev.stdenv.hostPlatform.system}.psst;
      };

      homeModules.default = import ./nix/home-module.nix self;
    };
}
