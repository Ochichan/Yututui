{
  description = "ytm-tui (ytt) — a fast terminal UI for YouTube Music";

  # Single input keeps the freeze simple: flake.lock pins exactly one nixpkgs revision, so
  # `nix run github:Ochichan/ytm-tui` reproduces the same build for everyone, forever.
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" "x86_64-darwin" "aarch64-darwin" ];
      forAllSystems = f:
        nixpkgs.lib.genAttrs systems (system: f system nixpkgs.legacyPackages.${system});
    in
    {
      packages = forAllSystems (system: pkgs:
        let
          lib = pkgs.lib;
          ytm-tui = pkgs.rustPlatform.buildRustPackage {
            pname = "ytm-tui";
            version = "1.6.21"; # keep in sync with Cargo.toml

            # Drop build artifacts and flake results from the copied source so the store path
            # stays small and rebuilds aren't invalidated by a local `target/`.
            src = lib.cleanSourceWith {
              src = ./.;
              filter = path: type:
                let base = baseNameOf path;
                in base != "target" && base != "result" && lib.cleanSourceFilter path type;
            };

            # No `outputHashes`: Cargo.lock has zero git sources, and the in-tree
            # `[patch.crates-io] ratatui-image = { path = "crates/ratatui-image" }` resolves
            # straight from `src` (the whole checkout is copied), so nothing is fetched.
            cargoLock.lockFile = ./Cargo.lock;

            nativeBuildInputs = [ pkgs.makeWrapper pkgs.pkg-config ];
            # native-tls links the system OpenSSL on Linux; on Darwin it uses Security.framework
            # (provided by the default SDK), so no extra buildInputs there.
            buildInputs = lib.optionals pkgs.stdenv.isLinux [ pkgs.openssl ];

            # ytt shells out to three tools. mpv + ffmpeg are hard deps we *prefix* onto PATH
            # (ours wins). yt-dlp is *suffixed* so a user's own, fresher yt-dlp takes precedence
            # — it breaks against YouTube changes often and must stay easy to update out-of-band.
            postFixup = ''
              wrapProgram $out/bin/ytt \
                --prefix PATH : ${lib.makeBinPath [ pkgs.mpv pkgs.ffmpeg ]} \
                --suffix PATH : ${lib.makeBinPath [ pkgs.yt-dlp ]}
            '';

            # The TUI has no headless mode; its tests are pure unit tests, which `cargo test`
            # runs in the checkPhase by default — keep them as the in-build gate.

            meta = {
              description = "A fast terminal UI for YouTube Music (search, radio, lyrics, album art, downloads)";
              homepage = "https://github.com/Ochichan/ytm-tui";
              license = lib.licenses.mit;
              mainProgram = "ytt";
              platforms = systems;
            };
          };
          # ---- Full GUI desktop app (darwin-only; docs/gui/04 §6) ----
          # tao/wry are non-target-gated optional deps, so building this on Linux would pull
          # webkit2gtk for a platform D9 excludes; the output is only exposed on darwin below.
          desktopSrc = lib.cleanSourceWith {
            src = ./.;
            filter = path: type:
              let base = baseNameOf path;
              in base != "target" && base != "result" && lib.cleanSourceFilter path type;
          };
          # Offline, lockfile-driven build of gui/dist. REGENERATE npmDepsHash on any
          # gui/package-lock.json change: `nix build .#ytm-tui-desktop` fails and prints the
          # correct hash to paste here (docs/gui/04 §9 risk 2).
          guiDist = pkgs.buildNpmPackage {
            pname = "ytm-tui-gui";
            version = "1.6.1"; # private GUI package version; not part of the release surface
            src = ./gui;
            npmDepsHash = "sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";
            dontNpmInstall = true;
            installPhase = ''
              runHook preInstall
              cp -r dist "$out"
              runHook postInstall
            '';
          };
          ytm-tui-desktop = pkgs.rustPlatform.buildRustPackage {
            pname = "ytm-tui-desktop";
            version = "1.6.21"; # keep the ytt-desktop binary version in sync with Cargo.toml
            src = desktopSrc;
            cargoLock.lockFile = ./Cargo.lock;
            nativeBuildInputs = [ pkgs.makeWrapper pkgs.pkg-config ];
            buildInputs = lib.optionals pkgs.stdenv.isLinux [ pkgs.openssl ];
            # Embed the prebuilt dist and require it (no stub page in a shipped binary).
            YTM_TUI_GUI_DIST = guiDist;
            YTM_TUI_REQUIRE_DIST = "1";
            cargoBuildFlags = [ "--features" "desktop" "--bin" "ytt-desktop" ];
            # Unit tests run via `cargo test` in CI/local; the feature build is the gate here.
            doCheck = false;
            postFixup = ''
              wrapProgram $out/bin/ytt-desktop \
                --prefix PATH : ${lib.makeBinPath [ pkgs.mpv pkgs.ffmpeg ]} \
                --suffix PATH : ${lib.makeBinPath [ pkgs.yt-dlp ]}
            '';
            meta = {
              description = "The full graphical desktop app for ytm-tui (macOS + Windows; this output is macOS).";
              homepage = "https://github.com/Ochichan/ytm-tui";
              license = lib.licenses.mit;
              mainProgram = "ytt-desktop";
              platforms = lib.platforms.darwin;
            };
          };
        in
        {
          default = ytm-tui;
          ytm-tui = ytm-tui;
          # Opt-in: `nix build .#ytm-tui-desktop` (darwin only — see the note above).
        } // lib.optionalAttrs pkgs.stdenv.isDarwin {
          ytm-tui-desktop = ytm-tui-desktop;
        });

      # `nix run github:Ochichan/ytm-tui` → launches ytt with mpv/ffmpeg/yt-dlp wrapped in.
      apps = forAllSystems (system: _pkgs: {
        default = {
          type = "app";
          program = "${self.packages.${system}.default}/bin/ytt";
        };
        ytm-tui = self.apps.${system}.default;
      });

      # `nix develop` → a shell for hacking on ytt: Rust toolchain + the runtime tools on PATH.
      devShells = forAllSystems (system: pkgs: {
        default = pkgs.mkShell {
          nativeBuildInputs = [ pkgs.pkg-config ];
          buildInputs = [
            pkgs.cargo
            pkgs.rustc
            pkgs.clippy
            pkgs.rust-analyzer
            pkgs.mpv
            pkgs.yt-dlp
            pkgs.ffmpeg
            # Frontend build for the desktop GUI (gui/): Vite + Svelte (docs/gui/04 §6).
            pkgs.nodejs_22
          ] ++ pkgs.lib.optionals pkgs.stdenv.isLinux [ pkgs.openssl ];
        };
      });

      formatter = forAllSystems (_system: pkgs: pkgs.nixpkgs-fmt);
    };
}
