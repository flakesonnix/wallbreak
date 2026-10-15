{
  description = "Experimental Rust image/video and one-scene wallpaper player for Wayland";
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/e11136d15d537efb924780f4bacbabd3340aac28";

  outputs = { nixpkgs, ... }:
    let
      system = "x86_64-linux";
      pkgs = import nixpkgs { inherit system; };
      wallpaper = pkgs.rustPlatform.buildRustPackage {
        pname = "wallpaper-rs";
        version = "0.1.0";
        src = pkgs.lib.fileset.toSource {
          root = ./.;
          fileset = pkgs.lib.fileset.unions [
            ./Cargo.toml
            ./Cargo.lock
            ./src
            ./tests
          ];
        };
        cargoLock.lockFile = ./Cargo.lock;
        nativeBuildInputs = [ pkgs.makeWrapper ];
        checkPhase = ''
          runHook preCheck
          export WALLPAPER_TEST_FFMPEG=${pkgs.lib.getExe pkgs.ffmpeg}
          cargo test --offline --locked --release --lib --bins --test cli --test scene -- --include-ignored
          cargo test --offline --locked --release --test headless
          runHook postCheck
        '';
        postInstall = ''
          wrapProgram "$out/bin/wallpaper-rs" \
            --prefix PATH : ${pkgs.lib.makeBinPath [ pkgs.ffmpeg ]}
        '';
        meta = {
          description = "Experimental Wayland image/video wallpaper player with one supported animated scene";
          license = pkgs.lib.licenses.mit;
          platforms = [ system ];
          mainProgram = "wallpaper-rs";
        };
      };
    in
    {
      packages.${system}.default = wallpaper;
      apps.${system}.default = {
        type = "app";
        program = "${wallpaper}/bin/wallpaper-rs";
        meta.description = wallpaper.meta.description;
      };
      checks.${system}.default = wallpaper;
      devShells.${system}.default = pkgs.mkShell {
        packages = with pkgs; [ cargo rustc rustfmt clippy ffmpeg sway grim ];
      };
    };
}
