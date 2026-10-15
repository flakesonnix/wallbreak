{
  description = "Experimental Rust image/video and one-scene wallpaper player for Wayland";
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/e11136d15d537efb924780f4bacbabd3340aac28";

  outputs = { nixpkgs, ... }:
    let
      system = "x86_64-linux";
      pkgs = import nixpkgs { inherit system; };
      wallbreak = pkgs.rustPlatform.buildRustPackage {
        pname = "wallbreak";
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
        buildFeatures = [ "gui" ];
        nativeBuildInputs = [ pkgs.makeWrapper ];
        checkPhase = ''
          runHook preCheck
          export WALLPAPER_TEST_FFMPEG=${pkgs.lib.getExe pkgs.ffmpeg}
          cargo test --offline --locked --release --lib --bins --test cli --test scene -- --include-ignored
          cargo test --offline --locked --release --test headless --test gui
          runHook postCheck
        '';
        postInstall = ''
          wrapProgram "$out/bin/wallbreak" \
            --prefix PATH : ${pkgs.lib.makeBinPath [ pkgs.ffmpeg ]}
          wrapProgram "$out/bin/wallbreak-gui" \
            --prefix PATH : ${pkgs.lib.makeBinPath [ pkgs.ffmpeg ]} \
            --prefix LD_LIBRARY_PATH : ${pkgs.lib.makeLibraryPath [
              pkgs.wayland
              pkgs.libGL
              pkgs.libxkbcommon
            ]}
        '';
        meta = {
          description = "Experimental Wayland image/video wallpaper player with one supported animated scene";
          license = pkgs.lib.licenses.mit;
          platforms = [ system ];
          mainProgram = "wallbreak";
        };
      };
    in
    {
      packages.${system}.default = wallbreak;
      apps.${system} = {
        default = {
          type = "app";
          program = "${wallbreak}/bin/wallbreak-gui";
          meta.description = "wallbreak control panel";
        };
        gui = {
          type = "app";
          program = "${wallbreak}/bin/wallbreak-gui";
          meta.description = "wallbreak control panel";
        };
        cli = {
          type = "app";
          program = "${wallbreak}/bin/wallbreak";
          meta.description = wallbreak.meta.description;
        };
      };
      checks.${system}.default = wallbreak;
      devShells.${system}.default = pkgs.mkShell {
        packages = with pkgs; [ cargo rustc rustfmt clippy ffmpeg sway grim ];
        shellHook = ''
          export LD_LIBRARY_PATH=${pkgs.lib.makeLibraryPath [
            pkgs.wayland
            pkgs.libGL
            pkgs.libxkbcommon
          ]}''${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}
        '';
      };
    };
}
