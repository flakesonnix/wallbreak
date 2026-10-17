# wallbreak

An independent, experimental Rust image/video and one-scene wallpaper player
for Linux Wayland compositors implementing `wlr-layer-shell`, tested with Sway.
GNOME has a static-image backend using `gsettings` and an optional GNOME 50
Shell bridge for videos and the supported animated scene. The bridge must be
installed and enabled separately; wallbreak never substitutes a foreground
window when it is unavailable.
It is not a full Wallpaper Engine clone. No Steam,
Wine, or Wallpaper Engine executable is launched.

Rust owns project selection, image decoding, bounded frame delivery, shared-memory
presentation, output selection, and decoder cleanup. Videos use an external
FFmpeg process, not a Rust video codec implementation. The supported animated
scene uses an independent CPU renderer and native project textures. The Nix
package supplies FFmpeg without changing the system configuration.

## Quick start

The easiest route is the control panel. From this directory:

```bash
nix run .
```

Or without a checkout:

```bash
nix run github:flakesonnix/wallbreak
```

The panel lists your outputs, detects installed Wallpaper Engine projects from
your local Steam libraries for one-click selection, accepts a dropped or typed
media path, shows an in-process preview, and starts/stops the wallpaper on the
selected output. To keep it permanently, install it into your profile:

```bash
nix profile install github:flakesonnix/wallbreak
wallbreak-gui
```

For scripts and automation, the CLI stays available:

```bash
nix run .#cli -- outputs
nix run .#cli -- projects
nix run .#cli -- inspect /absolute/path/to/image.jpg
nix run .#cli -- check /absolute/path/to/image.jpg
nix run .#cli -- play /absolute/path/to/image.jpg --output eDP-1
nix run .#cli -- play /absolute/path/to/video.mp4 --output eDP-1 --fps 30 --fit cover
```

Use an exact output name returned by `outputs`. Without `--output`, layer-shell
uses the first advertised output; GNOME animation uses the primary display. Each
process owns one output. `--fit contain` adds black bars instead of cropping.
Ctrl-C or SIGTERM removes the player's background surface and stops/reaps its
decoder. `--duration-seconds 15` limits a trial run. No startup service or desktop
configuration is installed, and existing wallpaper processes are not stopped.

On GNOME, use a direct image or select **Use project preview** explicitly for a
static project image. Leave output selection unset: GNOME applies its image to
all displays. The backend updates both light/dark image URIs and the picture
mode, then restores its prior values on Stop, normal GUI close, SIGINT/SIGTERM,
or a duration deadline. Settings changed by you or another app during playback
are retained. A per-user runtime lock rejects simultaneous static GNOME players.
Forced termination (SIGKILL), power loss, and external settings-service failures
can prevent restoration; those cases are not a persistent configuration service.

```bash
nix run .#cli -- play /absolute/path/to/image.jpg --duration-seconds 15
```

### GNOME Animation

The optional bridge supports GNOME 50 on Wayland. For an initial installation:

```bash
gnome-extensions install "$(nix run .#cli -- gnome-extension)"
gnome-extensions enable wallbreak@flakesonnix.github.io
```

These are explicit user actions, not automatic setup. Existing extension copies
are not overwritten by this install command. GNOME may require signing out and
back in to discover a new extension or reload previously imported code.

Check activation before playing:

```bash
gnome-extensions info wallbreak@flakesonnix.github.io
gdbus call --session --dest io.github.flakesonnix.Wallbreak \
  --object-path /io/github/flakesonnix/Wallbreak \
  --method io.github.flakesonnix.Wallbreak1.Info
```

Expect `State: ACTIVE` and protocol `1`. An `ERROR` state requires checking the
extension error, not repeatedly enabling it. A loaded error-state module cannot
be refreshed by simply toggling enable/disable in the same Shell session.

Select the original Eagle Flag project (without `--preview`) and use Play, or
run `play /absolute/path/to/eagleflag --duration-seconds 10`. The bridge owns a
dedicated renderer and displays its cloned actor below application windows.
Pressing Super keeps the animation visible behind the overview controls and in
the large workspace cards for the selected monitor. Small window thumbnails
retain GNOME's native layout. The extension does not animate the lock/login
screen or alter other monitors' backgrounds.
Stop removes that session; it does not replace the GNOME background settings.
The renderer paces its own frames without VSync, so hidden-source presentation
cannot add a VSync wait to graceful shutdown.

After updating the extension files, previously loaded Shell modules remain
cached until the next login; toggling enable/disable does not reload their code.
Overview behavior has simulated lifecycle/geometry regression coverage; live
GPU clipping and appearance still require checking after that reload.
Overview cards use additional cloned surfaces and a rounded clipping effect;
their GPU/memory cost has not been benchmarked.

GUI playback errors, including missing layer-shell, an unavailable GNOME bridge,
and decoder errors, remain visible in the panel. Stop requests graceful cleanup
before escalation; a forced kill is reported and cannot guarantee restoration.

Image formats: JPEG, PNG, WebP, BMP, GIF. Only a single static frame is decoded;
animated images and EXIF rotation are not implemented. Video input suffixes:
MP4/M4V, MKV, WebM, MOV, AVI, OGV, WMV, MPG/MPEG. FFmpeg still has to validate
their actual contents and codecs. Videos loop without audio. Presentation uses
software-decoded XRGB shared memory, not GPU decoding; non-square-pixel video
aspect correction is not implemented.

Diagnostics go to stderr only when requested: the CLI's global `-v`/`-vv`/`-vvv`
(or `WALLBREAK_LOG=error|warn|info|debug|trace` for every binary, including the
GUI) selects the level, defaulting to `warn`. Error reports print the full cause
chain, and decoded scene limitations are logged per scene.

Without Nix, use Rust 1.88+ and FFmpeg:

```bash
cargo build --locked --release
./target/release/wallbreak outputs
./target/release/wallbreak play /absolute/path/to/video.mp4 --ffmpeg /absolute/path/to/ffmpeg
```

## Wallpaper Engine Projects

`projects` lists every Wallpaper Engine project in your local Steam libraries
with its support status, so you can pick one instead of typing paths.
`inspect`, `check`, and `play` accept a project directory or `project.json` with
an explicit `type: "video"` or one supported `type: "scene"` profile, plus a
project-relative `file`. The selected media must resolve inside that project,
including through symlinks. Unknown types, malformed JSON, parent traversal, and
missing media fail rather than silently choosing another file or preview.
Backslash-separated project paths are not supported. Projects that omit `type`
are classified from the declared file instead; web and application projects are
reported but never executed.

Packed scenes are materialized read-only: when a scene project declares
`scene.json` but only ships a `scene.pkg` (PKGV container), the package is
extracted into `$XDG_CACHE_HOME/wallbreak/pkg` (or `~/.cache/wallbreak/pkg`) and
refreshed when the package changes. Entry names are validated against traversal,
duplicates, and size budgets; nothing is executed and the project directory is
never modified.

Supported scene content: layered orthographic image quads (in-plane rotation
allowed), composited back-to-front, with these material profiles:

- `flag` with `TINT=1`, three textures, and the project's three declared tint
  colors (Eagle Flag).
- `genericimage`/`genericimage2`/`genericimage4` with one rendered texture,
  optional brightness/alpha/power/scroll constants, and
  opaque/translucent/additive blending. Variant-only combos such as `VERSION`
  and disabled `LIGHTING`/`REFLECTION` are accepted and reported as notes.
- `flowimage` with two textures (base plus flow mask) or four textures (mask
  plus three parallax layers), with per-layer speeds.
- `swayimage` with two textures (base plus sway mask).

Textures may be native `.tex` (`TEXV0005`/`TEXB0003`, RGBA8 or BC3, raw or LZ4
mips, or packed JPEG/PNG mips) or raw PNG/JPEG/GIF/WebP/BMP images referenced
directly; animation plays its first frame. Depth testing/writing must be
disabled.

Scene content that is not rendered is skipped per object and reported as
limitations instead of failing the scene: 3D models, particle systems,
text/sound/light/sprite/solid objects, camera path animation, scripted
transforms, conditional visibility, bloom, and spritesheet or lighting
materials. A scene fails only when nothing renderable remains. Perspective
scenes without an orthogonal projection are approximated orthographically from
the camera distance and reported as such. For unsupported projects, explicit
`--preview` selects only the declared static preview and reports
`preview_only: true`:

```bash
project="$HOME/.local/share/Steam/steamapps/common/wallpaper_engine/projects/defaultprojects/beach"
nix run .#cli -- inspect "$project" --preview
nix run .#cli -- check "$project" --preview --width 640 --height 360
nix run .#cli -- play "$project" --preview --output eDP-1 --duration-seconds 15
```

The final command changes the chosen output temporarily. `inspect` only resolves
the input; `check` actually decodes frames without connecting to Wayland or
changing the desktop. Neither operation executes project scripts or binaries.

### Reverse-Engineering Evidence

The input contract was investigated from the locally installed Wallpaper Engine
2.8.42, Steam build 23967692, without executing it:

- `projects/defaultprojects/beach/project.json`: `type: "scene"`,
  `file: "scene.json"`, `preview: "preview.jpg"`.
- `projects/defaultprojects/arsenal/project.json`: the same entry/preview pattern.
- `projects/defaultprojects/corsair_o_tron/project.json`: `type: "web"`,
  `file: "index.html"`, `preview: "preview.gif"`.
- Of 19 installed default project manifests, 14 explicitly declare scenes, two
  declare web content, and three omit a type. No video project/media was found
  within that default collection. The video-project importer is therefore tested
  with synthetic projects, not claimed verified against a shipped video project.
- `projects/defaultprojects/eagleflag/project.json`: `type: "scene"`,
  `file: "scene.json"`, with project colors for scheme and two accents.
- `projects/defaultprojects/eagleflag/scene.json`: a 2200×1080 orthographic
  scene containing one zero-rotation `eagle` image object at
  `1082.000 550.906 0.000`, scaled by `1.100 1.100 1.000`.
- `projects/defaultprojects/eagleflag/models/eagle.json`: a 2048×1024 model
  using `materials/eagle.json`.
- `projects/defaultprojects/eagleflag/materials/eagle.json`: one `flag` pass
  with `TINT=1`, `Speed≈0.4`, `Strength≈0.3`, and textures
  `eagle`, `flag_normal`, and `cloth`.
- The three corresponding native `.tex` files use `TEXV0005`, `TEXI0001`, and
  `TEXB0003` with DXT5 payloads; their decoded dimensions match the shipped
  2048×1024, 32×32, and 512×512 images.

Earlier static analysis covered `distribution/wallpaper64.exe` resource
metadata and the selected `ui/dist/scripts/scripts.js` bundle. That does not
recover the native renderer: Ghidra was unavailable, bundle semantic coverage
was partial, and no native rendering code was decompiled. This project is a new
implementation of a narrower behavior, not recovered proprietary source.
No Wallpaper Engine code, assets, shaders, project files, or other binaries are
included here. Local ownership does not establish redistribution rights.

## Verification

```bash
cargo test --locked
cargo fmt --all --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --features gui
nix flake check .
```

The default Cargo test run explicitly ignores tests requiring external programs.
The Nix package runs the real FFmpeg decoding/loop/cleanup test as well as the
ordinary unit and public CLI tests. For the explicit real-provider lanes:

```bash
nix develop .
export WALLPAPER_TEST_FFMPEG="$(command -v ffmpeg)"
export WALLPAPER_TEST_SCENE_ROOT=/absolute/path/to/eagleflag
cargo test --locked --lib --bins --test cli --test scene -- --include-ignored

export WALLPAPER_TEST_SWAY="$(command -v sway)"
export WALLPAPER_TEST_SWAYMSG="$(command -v swaymsg)"
export WALLPAPER_TEST_GRIM="$(command -v grim)"
cargo test --locked --test headless -- --ignored --nocapture

# Real gsettings round trips in a temporary keyfile backend, not your desktop:
export WALLBREAK_TEST_GSETTINGS="$(command -v gsettings)"
cargo test --locked --features gui --test gnome -- --ignored
```

The local scene tests require an explicitly selected Eagle Flag project and do
not bundle its textures. They check native texture decoding, profile resolution,
animated rendering, and preview similarity. The headless test starts a separate
software-rendered Sway instance with its own runtime directory and
configuration. It checks output discovery, actual image pixels, changing video
pixels, resize, scale, automatic stop, SIGTERM, surface removal, and, when a
scene root is supplied, changing animated-scene pixels. It never connects to
the existing desktop's socket. It generates its own image/video media; no
commercial fixture is required. X11, custom scene shaders (for example
`cloudsbg`, `retro`, and `spritesheet` variants), JavaScript scenes, interactive
wallpapers, and full Wallpaper Engine compatibility remain unsupported.

Frames are admitted before allocation, with a 64 MiB physical-frame ceiling,
at most three shared-memory presentation buffers, one pending presentation
frame, and a latest-frame-only decoder slot. The reader can hold one additional
frame while decoding. Image decoding has an independent 64 MiB allocation
budget. Native scene textures have a separate 64 MiB decoded budget. The scene
renderer may use up to eight threads; high-resolution scenes can therefore use
substantial sustained CPU. FFmpeg has a 64 MiB per-allocation limit and two
decoder threads, not a process-wide memory cap. It may access local files but
network protocols are disabled. This is not a sandbox for untrusted media;
decoder internals, total CPU/memory usage, and 4K/60-fps performance are not
bounded or benchmarked here.

## Releases

Pushing a `v*` tag builds a release tarball with both binaries and attaches it
to a GitHub release:

```bash
git tag v0.1.0
git push origin v0.1.0
```

You can build the same tarball locally with `scripts/package-release.sh`. The
tarball does not bundle FFmpeg or the GUI's OpenGL/Wayland runtime libraries;
prefer the Nix flake for a complete installation.

## License

MIT; see [LICENSE](LICENSE).
