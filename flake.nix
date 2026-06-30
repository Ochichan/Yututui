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
            version = "1.5.6"; # keep in sync with Cargo.toml

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
        in
        {
          default = ytm-tui;
          ytm-tui = ytm-tui;
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
          ] ++ pkgs.lib.optionals pkgs.stdenv.isLinux [ pkgs.openssl ];
        };
      });

      formatter = forAllSystems (_system: pkgs: pkgs.nixpkgs-fmt);
    };
}
