{
  description = "Experimental Rust image/video and one-scene wallpaper player for Wayland";
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/e11136d15d537efb924780f4bacbabd3340aac28";

  outputs = { nixpkgs, ... }:
    let
      system = "x86_64-linux";
      pkgs = import nixpkgs { inherit system; };
      schemaRoot = "${pkgs.gsettings-desktop-schemas}/share/gsettings-schemas/${pkgs.gsettings-desktop-schemas.name}";
      extensionUuid = "wallbreak@flakesonnix.github.io";
      guiLibraries = pkgs.lib.makeLibraryPath [ pkgs.wayland pkgs.libGL pkgs.libxkbcommon ];
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
            ./extensions
          ];
        };
        cargoLock.lockFile = ./Cargo.lock;
        buildFeatures = [ "gui" ];
        nativeBuildInputs = [ pkgs.makeWrapper pkgs.zip pkgs.nodejs_24 ];
        checkPhase = ''
          runHook preCheck
          export WALLPAPER_TEST_FFMPEG=${pkgs.lib.getExe pkgs.ffmpeg}
          export WALLBREAK_TEST_GSETTINGS=${pkgs.glib.bin}/bin/gsettings
          export WALLBREAK_TEST_DBUS_DAEMON=${pkgs.dbus}/bin/dbus-daemon
          export GSETTINGS_SCHEMA_DIR=${schemaRoot}/glib-2.0/schemas
          export WALLBREAK_TEST_DBUS_DAEMON=${pkgs.dbus}/bin/dbus-daemon
          cat > ./wallbreak-test-session.conf <<'EOF'
          <!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN" "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
          <busconfig>
            <type>session</type>
            <keep_umask/>
            <listen>unix:tmpdir=/tmp</listen>
            <policy context="default">
              <allow send_destination="*"/>
              <allow eavesdrop="true"/>
              <allow eavesdrop="true"/>
              <allow own="*"/>
            </policy>
          </busconfig>
          EOF
          export WALLBREAK_TEST_DBUS_CONFIG="$PWD/wallbreak-test-session.conf"
          cargo test --offline --locked --release --features gui --lib --bins --test cli --test scene -- --include-ignored
          cargo test --offline --locked --release --features gui --test player --test headless --test gui --test renderer --test gnome_bridge
          cargo test --offline --locked --release --features gui --test gnome -- --ignored
          node --experimental-vm-modules --test tests/gnome-extension.test.mjs
          runHook postCheck
        '';
        postInstall = ''
          mkdir -p "$out/share/gnome-shell/extensions" "$out/share/wallbreak"
          cp -r "extensions/${extensionUuid}" "$out/share/gnome-shell/extensions/"
          zip -j "$out/share/wallbreak/${extensionUuid}.zip" "extensions/${extensionUuid}"/*
          wrapProgram "$out/bin/wallbreak" \
            --set-default WALLBREAK_GNOME_EXTENSION_ZIP "$out/share/wallbreak/${extensionUuid}.zip" \
            --prefix PATH : ${pkgs.lib.makeBinPath [ pkgs.ffmpeg pkgs.glib.bin ]} \
            --prefix XDG_DATA_DIRS : ${schemaRoot} \
            --prefix GIO_EXTRA_MODULES : ${pkgs.lib.getLib pkgs.dconf}/lib/gio/modules \
            --prefix LD_LIBRARY_PATH : ${pkgs.lib.makeLibraryPath [
              pkgs.wayland
            ]}
          wrapProgram "$out/bin/wallbreak-gui" \
            --set-default WALLBREAK_GNOME_EXTENSION_ZIP "$out/share/wallbreak/${extensionUuid}.zip" \
            --prefix PATH : ${pkgs.lib.makeBinPath [ pkgs.ffmpeg pkgs.glib.bin ]} \
            --prefix XDG_DATA_DIRS : ${schemaRoot} \
            --prefix GIO_EXTRA_MODULES : ${pkgs.lib.getLib pkgs.dconf}/lib/gio/modules \
            --prefix LD_LIBRARY_PATH : ${pkgs.lib.makeLibraryPath [
              pkgs.wayland
              pkgs.libGL
              pkgs.libxkbcommon
            ]}
          wrapProgram "$out/bin/wallbreak-renderer" \
            --prefix PATH : ${pkgs.lib.makeBinPath [ pkgs.ffmpeg ]} \
            --prefix LD_LIBRARY_PATH : ${guiLibraries}
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
        packages = with pkgs; [ cargo rustc rustfmt clippy ffmpeg sway grim glib.bin dbus nodejs_24 zip ];
        shellHook = ''
          export XDG_DATA_DIRS=${schemaRoot}''${XDG_DATA_DIRS:+:$XDG_DATA_DIRS}
          export GIO_EXTRA_MODULES=${pkgs.lib.getLib pkgs.dconf}/lib/gio/modules''${GIO_EXTRA_MODULES:+:$GIO_EXTRA_MODULES}
          export LD_LIBRARY_PATH=${pkgs.lib.makeLibraryPath [
            pkgs.wayland
            pkgs.libGL
            pkgs.libxkbcommon
          ]}''${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}
        '';
      };
    };
}
