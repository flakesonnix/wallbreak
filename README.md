# wallpaper-rs

An independent, experimental Rust image/video wallpaper player for Linux
Wayland compositors implementing `wlr-layer-shell`, tested with Sway. It is not
a full Wallpaper Engine clone or a REA analysis provider. No Steam, Wine, or
Wallpaper Engine executable is launched.

Rust owns project selection, image decoding, bounded frame delivery, shared-memory
presentation, output selection, and decoder cleanup. Videos use an external
FFmpeg process, not a Rust video codec implementation. The Nix package supplies
FFmpeg without changing the system configuration.

## Use

From this directory:

```bash
nix build path:.
nix run path:. -- outputs
nix run path:. -- inspect /absolute/path/to/image.jpg
nix run path:. -- check /absolute/path/to/image.jpg
nix run path:. -- play /absolute/path/to/image.jpg --output eDP-1
nix run path:. -- play /absolute/path/to/video.mp4 --output eDP-1 --fps 30 --fit cover
```

Use an exact output name returned by `outputs`. Without `--output`, the first
compositor-advertised output is selected, not an inferred primary monitor. Each
process owns one output. `--fit contain` adds black bars instead of cropping.
Ctrl-C or SIGTERM removes the player's background surface and stops/reaps its
decoder. `--duration-seconds 15` limits a trial run. No startup service or desktop
configuration is installed, and existing wallpaper processes are not stopped.

Image formats: JPEG, PNG, WebP, BMP, GIF. Only a single static frame is decoded;
animated images and EXIF rotation are not implemented. Video input suffixes:
MP4/M4V, MKV, WebM, MOV, AVI, OGV, WMV, MPG/MPEG. FFmpeg still has to validate
their actual contents and codecs. Videos loop without audio. Presentation uses
software-decoded XRGB shared memory, not GPU decoding; non-square-pixel video
aspect correction is not implemented.

Without Nix, use Rust 1.88+ and FFmpeg:

```bash
cargo build --locked --release
./target/release/wallpaper-rs outputs
./target/release/wallpaper-rs play /absolute/path/to/video.mp4 --ffmpeg /absolute/path/to/ffmpeg
```

## Wallpaper Engine Projects

`inspect`, `check`, and `play` accept a project directory or `project.json` with
an explicit `type: "video"` and a project-relative `file`. The selected media
must resolve inside that project, including through symlinks. Unknown types,
malformed JSON, parent traversal, and missing media fail rather than silently
choosing another file. Backslash-separated project paths are not supported.

Scene, web, and application wallpapers are not supported. They do not become
image or video wallpapers because a preview exists. Explicit `--preview` selects
only the declared static preview and reports `preview_only: true`:

```bash
project="$HOME/.local/share/Steam/steamapps/common/wallpaper_engine/projects/defaultprojects/beach"
nix run path:. -- inspect "$project" --preview
nix run path:. -- check "$project" --preview --width 640 --height 360
nix run path:. -- play "$project" --preview --output eDP-1 --duration-seconds 15
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

Earlier REA static analysis covered `distribution/wallpaper64.exe` resource
metadata and the selected `ui/dist/scripts/scripts.js` bundle. That does not
recover the native renderer: Ghidra was unavailable, bundle semantic coverage
was partial, and no native rendering code was decompiled. This MVP is a new
implementation of a narrower behavior, not recovered proprietary source.
No Wallpaper Engine code, assets, shaders, project files, or other binaries are
included here. Local ownership does not establish redistribution rights.

## Verification

```bash
cargo test --locked
cargo fmt --all --check
cargo clippy --locked --all-targets -- -D warnings
nix flake check path:.
```

The default Cargo test run explicitly ignores tests requiring external programs.
The Nix package runs the real FFmpeg decoding/loop/cleanup test as well as the
ordinary unit and public CLI tests. For the explicit real-provider lanes:

```bash
nix develop path:.
export WALLPAPER_TEST_FFMPEG="$(command -v ffmpeg)"
cargo test --locked --lib --bins --test cli -- --include-ignored

export WALLPAPER_TEST_SWAY="$(command -v sway)"
export WALLPAPER_TEST_SWAYMSG="$(command -v swaymsg)"
export WALLPAPER_TEST_GRIM="$(command -v grim)"
cargo test --locked --test headless -- --ignored --nocapture
```

The headless test starts a separate software-rendered Sway instance with its own
runtime directory and configuration. It checks output discovery, actual image
pixels, changing video pixels, resize, scale, automatic stop, SIGTERM, and surface
removal. It never connects to the existing desktop's socket. It generates its
own media; no commercial fixture is required. X11, GNOME, scene shaders,
JavaScript scenes, packed `scene.pkg` content, interactive wallpapers, and
full Wallpaper Engine compatibility are outside this MVP.

Frames are admitted before allocation, with a 64 MiB physical-frame ceiling,
at most three shared-memory presentation buffers, one pending presentation
frame, and a latest-frame-only decoder slot. The reader can hold one additional
frame while decoding. Image decoding has an independent 64 MiB allocation
budget. FFmpeg has a 64 MiB per-allocation limit and two decoder threads, not a
process-wide memory cap. It may access local files but network protocols are
disabled. This is not a sandbox for untrusted media; decoder internals, total
CPU/memory usage, and 4K/60-fps performance are not bounded or benchmarked here.
