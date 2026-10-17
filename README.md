# wallbreak

An independent, experimental Rust image/video and layered-scene wallpaper player
for Linux Wayland compositors implementing `wlr-layer-shell`, tested with Sway.
GNOME has a static-image backend using `gsettings` and an optional GNOME 50
Shell bridge for videos and supported animated scenes. The bridge must be
installed and enabled separately; wallbreak never substitutes a foreground
window when it is unavailable.
It is not a full Wallpaper Engine clone. No Steam,
Wine, or Wallpaper Engine executable is launched.

Rust owns project selection, image decoding, bounded frame delivery, rendering,
presentation, output selection, and decoder cleanup. Videos use
an external FFmpeg process for CPU decoding, not a Rust video codec implementation
or hardware decoding. Supported scenes use native project textures and independent
CPU/GPU renderers; the active path depends on the backend (see
[Rendering Backends](#rendering-backends)). The Nix package supplies FFmpeg without
changing the system configuration.

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

Select a supported scene project (without `--preview`) and use Play, for example
`play /absolute/path/to/eagleflag --duration-seconds 10`. Videos also use this
bridge. It owns a dedicated renderer and displays its cloned actor below
application windows.
Pressing Super keeps the animation visible behind the overview controls and in
the large workspace cards for the selected monitor. Small window thumbnails
retain GNOME's native layout. The extension does not animate the lock/login
screen or alter other monitors' backgrounds.
Stop removes that session; it does not replace the GNOME background settings.
The renderer paces its own frames without VSync, so hidden-source presentation
cannot add a VSync wait to graceful shutdown.
The desktop clone stays attached and privacy-gated during overview transitions;
GNOME's native parent groups control the crossfade instead of the bridge hiding
the desktop animation as soon as the overview starts opening.

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
CPU-decoded frames: XRGB shared memory on layer-shell, or I420/YUV texture uploads
and GPU color conversion in the GNOME renderer. Neither path implements hardware
video decoding; non-square-pixel video aspect correction is not implemented.

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

That default build is CLI-only. Add `--features gui` to build the control panel
and `wallbreak-renderer`; the GNOME Shell bridge still requires separate
installation and activation.

## Rendering Backends

| Path | Current implementation |
| --- | --- |
| GNOME bridge renderer | `src/renderer.rs` uses `glow` paint callbacks and `src/gpu.rs` to draw supported scenes, decoded images, and uploaded I420/YUV video planes. Video YUV-to-RGB conversion runs in a shader. |
| GNOME static images | Public image and `--preview` playback uses `gsettings`, not the bridge renderer's image draw path. |
| Sway / `wlr-layer-shell` | `src/display.rs` presents CPU-rendered/decoded frames through `wl_shm` XRGB buffers. It does not use the GPU renderer. |
| CLI `check` / GUI previews | `MediaWorker` produces CPU frames. The GUI displays a finished preview as an egui texture, not through the scene/YUV GPU callbacks. |

The GPU renderer requires OpenGL 3.1+ or OpenGL ES 3.0+ in the bridge-owned
eframe window. Normal window exit deletes its owned GL resources through
`on_exit`. This is not a standalone layer-shell EGL presenter: that backend is
not implemented, and no `--software` flag exists. A GL context may itself use a
software driver; using `glow` does not establish hardware acceleration or a
performance improvement.

Scene textures and still images are decoded on the CPU before upload. FFmpeg
still performs video decoding, frame-rate filtering, cropping/scaling, and padding
on the CPU before delivering I420 planes. Its scale filter explicitly normalizes
GPU video output with `out_color_matrix=bt709:out_range=tv`, matching the shader's
BT.709 limited-range conversion. This handles matrix/range conversion for
correctly tagged BT.601/BT.709 full/limited-range inputs, not general color
management: untagged or incorrect metadata, HDR transfer functions/color primaries,
and chroma positioning remain limitations. The CPU and GPU scene renderers share
validated scene/material data, not a guarantee of exact pixel parity or full
Wallpaper Engine compatibility.

## Wallpaper Engine Projects

`projects` lists every Wallpaper Engine project in your local Steam libraries
with its support status, so you can pick one instead of typing paths.
`inspect`, `check`, and `play` accept a project directory or `project.json` with
an explicit `type: "video"` or a supported `type: "scene"` profile, plus a
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
cargo clippy --locked --features gui --all-targets -- -D warnings
cargo test --locked --features gui
nix flake check .
```

The default Cargo test runs ignore tests requiring external programs or an
explicitly selected EGL library. The Nix package check is configured to include
the real FFmpeg decoding/color/loop/cleanup tests and both ignored EGL tests via
`--features gui --lib -- --include-ignored`, alongside ordinary unit and public
CLI tests. Its EGL lane selects the explicit Mesa vendor library and forces
software rendering. For the explicit real-provider lanes:

```bash
nix develop .
export WALLPAPER_TEST_FFMPEG="$(command -v ffmpeg)"
export WALLPAPER_TEST_SCENE_ROOT=/absolute/path/to/eagleflag
cargo test --locked --lib --bins --test cli --test scene -- --include-ignored

# Real offscreen EGL without a compositor or desktop:
# nix develop exports WALLBREAK_TEST_EGL as an absolute libEGL.so path.
LIBGL_ALWAYS_SOFTWARE=1 cargo test --locked --features gui --lib gpu::tests::real_surfaceless -- --ignored --nocapture

export WALLPAPER_TEST_SWAY="$(command -v sway)"
export WALLPAPER_TEST_SWAYMSG="$(command -v swaymsg)"
export WALLPAPER_TEST_GRIM="$(command -v grim)"
cargo test --locked --test headless -- --ignored --nocapture

# Real gsettings round trips in a temporary keyfile backend, not your desktop:
export WALLBREAK_TEST_GSETTINGS="$(command -v gsettings)"
cargo test --locked --features gui --test gnome -- --ignored
```

The dev shell supplies the EGL loader but leaves driver selection to the host,
unlike the package check's explicit Mesa vendor selection. Outside that shell,
set `WALLBREAK_TEST_EGL` to an absolute EGL 1.5 library path with Mesa surfaceless
support; without it the ignored GPU tests skip rather than verify rendering.

The two `gpu::tests::real_surfaceless_*` tests create real desktop GL and GLES
contexts, compile/link all production shaders, and read back rendered pixels.
They cover image cover/contain and alpha compositing; a padded generic scene
texture with opaque/translucent/additive blending; and I420 uploads at 6x4 and
3x3, unpack alignment/state, resizing, invalid inputs, GL failures, and recovery.
`flag`, `flowimage` (including layered flow), and `swayimage` shaders are compiled,
but do not yet have golden-frame or CPU/GPU pixel-parity coverage.

The real FFmpeg color test compares a CPU equivalent of the video shader's YUV
conversion with BGRA output and known color patches for tagged BT.601/BT.709
full/limited-range inputs, in both fit modes. Targeted EGL checks passed on Mesa
llvmpipe exposing desktop OpenGL 4.6 and OpenGL ES 3.2; the real FFmpeg lane also
passed via `nix develop`. The complete Nix package build/check and `nix flake
check .` also passed, including software EGL, real FFmpeg, isolated gsettings,
and simulated Shell lifecycle checks. These results do not establish
hardware-driver behavior, live GNOME appearance, or exact scene parity. Tests
requiring local commercial scene assets or an isolated Sway compositor were not
executed in this verification run.

The local scene and headless Sway checks exercise the CPU/SHM path, not the GNOME
GPU renderer. Geometry and paint-callback lifecycle tests alone do not establish
GPU/CPU pixel parity or live GNOME appearance; remaining checks are TODOs below.

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

Frames are admitted before allocation, with a 64 MiB physical-frame ceiling.
The CPU/SHM path has at most three shared-memory presentation buffers, one pending
presentation frame, and a latest-frame-only decoder slot. The reader can hold one
additional frame while decoding. Direct-image decoding has separate 64 MiB
decoder-allocation and RGBA-output limits. Native `.tex` files have a 64 MiB
encoded-file limit and a 64 MiB sum-of-declared-RGBA-mip-expansions limit. Scene
loading separately checks a 128 MiB aggregate decoded-RGBA budget; raw scene-image
decoding can allocate before its RGBA size check. These checks are not a cap on
transient allocations or copies. The scene CPU renderer may use up to eight
threads; high-resolution scenes can therefore use substantial sustained CPU. GPU
textures, mipmaps, driver allocations, and GNOME clone surfaces add memory beyond
these CPU budgets; they are not a VRAM or total-process memory cap. FFmpeg has a
64 MiB per-allocation limit and two decoder threads, not a process-wide memory
cap. It may access local files but network protocols are disabled. This is not a
sandbox for untrusted media; decoder internals, total CPU/memory usage, and
4K/60-fps performance are not bounded or benchmarked here.

## TODO

- [ ] Add an EGL GPU presenter for `wlr-layer-shell` with automatic CPU/SHM
  fallback and an explicit `--software` override.
- [ ] Check real GNOME 50 playback after a Shell reload: scene/video appearance,
  fit/scaling, monitor selection, desktop/overview clipping, and Stop cleanup.
- [ ] Add fixed-time CPU/GPU scene comparisons for `flag`, flow, and sway, then
  extend parity coverage across material constants, transforms, blending, and
  fit modes. Shader compilation and generic-scene readback do not establish full
  scene parity.
- [ ] Define and test behavior for untagged/incorrect video color metadata, HDR
  transfer functions/color primaries, and chroma positioning; matrix/range
  normalization alone is not full color management.
- [ ] Check RGBA expansion before decoding raw/packed scene images and reduce
  transient texture copies during scene loading.
- [ ] Measure equivalent CPU/GPU scene and video workloads: CPU time, CPU/GPU
  frame time, and peak process-family/GPU memory, including FFmpeg and GNOME clones.
  Record resolution, FPS, driver, and cold/warm runs before making performance
  claims; no comparative measurements are available here.

## Releases

Pushing a `v*` tag builds a release tarball with both binaries and attaches it
to a GitHub release:

```bash
git tag v0.1.0
git push origin v0.1.0
```

You can build the same tarball locally with `scripts/package-release.sh`. It
contains `wallbreak` and `wallbreak-gui`, but currently omits `wallbreak-renderer`
and the extension archive required for GNOME animation. It also does not bundle
FFmpeg or the GUI's OpenGL/Wayland runtime libraries; prefer the Nix flake for a
complete installation.

## License

MIT; see [LICENSE](LICENSE).
