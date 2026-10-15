use std::{fs, process::Command};

use image::{Rgba, RgbaImage};

const BINARY: &str = env!("CARGO_BIN_EXE_wallpaper-rs");

#[test]
fn decodes_real_image_without_wayland_or_ffmpeg() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("sample.png");
    RgbaImage::from_pixel(16, 16, Rgba([17, 53, 89, 255]))
        .save(&path)
        .unwrap();
    let output = Command::new(BINARY)
        .arg("check")
        .arg(path)
        .args(["--width", "32", "--height", "18"])
        .env_remove("WAYLAND_DISPLAY")
        .env_remove("WAYLAND_SOCKET")
        .env("PATH", "/no-helpers")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["decoded_frames"], 1);
    assert_eq!(result["desktop_modified"], false);
}

#[test]
fn scene_project_requires_explicit_preview_and_preserves_source() {
    let directory = tempfile::tempdir().unwrap();
    let source = r#"{"file":"scene.json","preview":"preview.png","type":"scene","general":{"properties":{"schemecolor":{"type":"color","value":"0 0 0"},"flagcolor1":{"type":"color","value":"1 1 1"},"flagcolor2":{"type":"color","value":"1 0 0"}}}}"#;
    fs::write(directory.path().join("project.json"), source).unwrap();
    fs::write(
        directory.path().join("scene.json"),
        r#"{"camera":{"center":"0 0 -1","eye":"0 0 0","up":"0 1 0"},"general":{"bloom":true,"clearcolor":"0 0 0","orthogonalprojection":{"width":2,"height":2}},"objects":[{"angles":"0 0 0","id":1,"image":"models/a.json","name":"a","origin":"1 1 0","scale":"1 1 1"}]}"#,
    )
    .unwrap();
    RgbaImage::new(2, 2)
        .save(directory.path().join("preview.png"))
        .unwrap();
    let rejected = Command::new(BINARY)
        .arg("inspect")
        .arg(directory.path())
        .output()
        .unwrap();
    assert!(!rejected.status.success());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("unsupported"));
    let preview = Command::new(BINARY)
        .arg("check")
        .arg(directory.path())
        .arg("--preview")
        .output()
        .unwrap();
    assert!(
        preview.status.success(),
        "{}",
        String::from_utf8_lossy(&preview.stderr)
    );
    let result: serde_json::Value = serde_json::from_slice(&preview.stdout).unwrap();
    assert_eq!(result["media"]["preview_only"], true);
    assert_eq!(
        fs::read_to_string(directory.path().join("project.json")).unwrap(),
        source
    );
}

#[test]
fn rejects_invalid_dimensions_corrupt_media_and_invalid_cli_limits() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("bad.png");
    fs::write(&path, b"not an image").unwrap();
    for options in [vec![], vec!["--width", "0"], vec!["--width", "4294967295"]] {
        let output = Command::new(BINARY)
            .arg("check")
            .arg(&path)
            .args(options)
            .output()
            .unwrap();
        assert!(!output.status.success());
    }
    for fps in ["0", "61", "-1"] {
        let output = Command::new(BINARY)
            .arg("play")
            .arg(&path)
            .args(["--fps", fps])
            .output()
            .unwrap();
        assert!(!output.status.success());
    }
}
