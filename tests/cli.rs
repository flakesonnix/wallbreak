use std::{fs, path::Path, process::Command};

use image::{Rgba, RgbaImage};

const BINARY: &str = env!("CARGO_BIN_EXE_wallbreak");

fn scene_fixture(root: &Path) {
    fs::create_dir_all(root.join("models")).unwrap();
    fs::create_dir_all(root.join("materials")).unwrap();
    fs::write(root.join("scene.json"), r#"{"camera":{"center":"0 0 -1","eye":"0 0 0","up":"0 1 0"},"general":{"bloom":false,"clearcolor":"0 0 0","orthogonalprojection":{"width":2,"height":2}},"objects":[{"angles":"0 0 0","id":1,"image":"models/quad.json","name":"quad","origin":"1 1 0","scale":"1 1 1"}]}"#).unwrap();
    fs::write(
        root.join("models/quad.json"),
        r#"{"height":2,"width":2,"material":"materials/quad.json"}"#,
    )
    .unwrap();
    fs::write(root.join("materials/quad.json"), r#"{"passes":[{"shader":"genericimage","depthtest":"disabled","depthwrite":"disabled","textures":["pixel"]}]}"#).unwrap();
    let mut texture = Vec::from(b"TEXV0005\0TEXI0001\0".as_slice());
    for word in [0u32, 0, 1, 1, 1, 1, 0] {
        texture.extend_from_slice(&word.to_le_bytes());
    }
    texture.extend_from_slice(b"TEXB0003\0");
    for word in [1u32, u32::MAX, 1, 1, 1, 0, 4, 4] {
        texture.extend_from_slice(&word.to_le_bytes());
    }
    texture.extend_from_slice(&[24, 80, 160, 255]);
    fs::write(root.join("materials/pixel.tex"), texture).unwrap();
}

#[test]
fn direct_scene_json_decodes_without_a_manifest_or_helpers() {
    let directory = tempfile::tempdir().unwrap();
    scene_fixture(directory.path());
    let scene = directory.path().join("scene.json");
    let output = Command::new(BINARY)
        .arg("check")
        .arg(&scene)
        .args(["--width", "8", "--height", "8", "--frames", "2"])
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
    assert_eq!(result["decoded_frames"], 2);
    assert_eq!(result["desktop_modified"], false);
    assert_eq!(result["media"]["kind"], "scene");
    assert_eq!(result["media"]["path"], scene.to_str().unwrap());
    assert!(result["media"]["project_path"].is_null());
    assert!(result["media"]["declared_file"].is_null());
    assert_eq!(result["media"]["preview_only"], false);
    let preview = Command::new(BINARY)
        .arg("inspect")
        .arg(&scene)
        .arg("--preview")
        .output()
        .unwrap();
    assert!(!preview.status.success());
    assert!(String::from_utf8_lossy(&preview.stderr).contains("standalone scene.json"));
    assert!(!directory.path().join("project.json").exists());
}

#[test]
fn direct_scene_json_retains_project_provenance_and_explicit_preview() {
    let directory = tempfile::tempdir().unwrap();
    scene_fixture(directory.path());
    let scene = directory.path().join("scene.json");
    let project = directory.path().join("project.json");
    let source =
        r#"{"title":"Direct scene","type":"scene","file":"./scene.json","preview":"preview.png"}"#;
    fs::write(&project, source).unwrap();
    RgbaImage::from_pixel(2, 2, Rgba([9, 27, 81, 255]))
        .save(directory.path().join("preview.png"))
        .unwrap();
    let direct = wallbreak::project::resolve(&scene, false).unwrap();
    let manifest = wallbreak::project::resolve(&project, false).unwrap();
    assert_eq!(direct.path, manifest.path);
    assert_eq!(direct.source_path, scene);
    assert_eq!(manifest.source_path, project);
    assert_eq!(direct.project_path, Some(project.clone()));
    assert_eq!(direct.declared_file.as_deref(), Some("./scene.json"));
    for preview in [false, true] {
        let mut command = Command::new(BINARY);
        command
            .arg("check")
            .arg(&scene)
            .args(["--width", "8", "--height", "8"]);
        if preview {
            command.arg("--preview");
        }
        let output = command
            .env_remove("WAYLAND_DISPLAY")
            .env_remove("WAYLAND_SOCKET")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(result["media"]["project_path"], project.to_str().unwrap());
        assert_eq!(result["media"]["preview_only"], preview);
        assert_eq!(
            result["media"]["declared_file"],
            if preview {
                "preview.png"
            } else {
                "./scene.json"
            }
        );
        assert_eq!(
            result["media"]["kind"],
            if preview { "image" } else { "scene" }
        );
        assert_eq!(result["desktop_modified"], false);
    }
    let direct_preview = wallbreak::project::resolve(&scene, true).unwrap();
    assert_eq!(direct_preview.source_path, scene);
    assert_eq!(direct_preview.path, directory.path().join("preview.png"));
    assert_eq!(fs::read_to_string(project).unwrap(), source);
}

#[test]
fn direct_scene_json_never_switches_to_another_declared_source() {
    let directory = tempfile::tempdir().unwrap();
    scene_fixture(directory.path());
    let scene = directory.path().join("scene.json");
    let source = fs::read(&scene).unwrap();
    fs::write(directory.path().join("other.json"), &source).unwrap();
    fs::write(directory.path().join("clip.mp4"), b"must not be selected").unwrap();
    RgbaImage::new(2, 2)
        .save(directory.path().join("preview.png"))
        .unwrap();
    for (manifest, reason) in [
        (
            r#"{"type":"scene","file":"other.json","preview":"preview.png"}"#,
            "selected scene.json",
        ),
        (
            r#"{"type":"scene","file":"../scene.json","preview":"preview.png"}"#,
            "relative path",
        ),
        (
            r#"{"type":"video","file":"clip.mp4","preview":"preview.png"}"#,
            "declare a scene project",
        ),
        (
            r#"{"type":"web","file":"index.html","preview":"preview.png"}"#,
            "declare a scene project",
        ),
        ("{", "invalid project.json"),
        ("[]", "JSON object"),
        (
            r#"{"type":"scene","file":"scene.json","general":{"properties":null}}"#,
            "invalid project.json",
        ),
    ] {
        let project = directory.path().join("project.json");
        fs::write(&project, manifest).unwrap();
        for preview in [false, true] {
            let mut command = Command::new(BINARY);
            command.arg("inspect").arg(&scene);
            if preview {
                command.arg("--preview");
            }
            let output = command.output().unwrap();
            assert!(!output.status.success(), "{manifest}");
            assert!(
                String::from_utf8_lossy(&output.stderr).contains(reason),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        assert_eq!(fs::read_to_string(project).unwrap(), manifest);
        assert_eq!(fs::read(&scene).unwrap(), source);
    }
}

#[test]
fn direct_flag_scene_requires_declared_project_colors() {
    let directory = tempfile::tempdir().unwrap();
    scene_fixture(directory.path());
    let scene = directory.path().join("scene.json");
    fs::write(directory.path().join("materials/quad.json"), r#"{"passes":[{"shader":"flag","combos":{"TINT":1},"depthtest":"disabled","depthwrite":"disabled","textures":["pixel","pixel","pixel"],"constantshadervalues":{"Speed":0.5,"Strength":0.5},"usershadervalues":{"schemecolor":"color1","flagcolor1":"color2","flagcolor2":"color3"}}]}"#).unwrap();
    let missing = Command::new(BINARY)
        .arg("inspect")
        .arg(&scene)
        .output()
        .unwrap();
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("schemecolor"));
    fs::write(directory.path().join("project.json"), r#"{"type":"scene","file":"scene.json","general":{"properties":{"schemecolor":{"type":"color","value":"1 0 0"},"flagcolor1":{"type":"color","value":"0 1 0"},"flagcolor2":{"type":"color","value":"0 0 1"}}}}"#).unwrap();
    let output = Command::new(BINARY)
        .arg("check")
        .arg(&scene)
        .args(["--width", "8", "--height", "8", "--frames", "2"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let colors = &result["media"]["scene_profile"]["layers"][0]["material"]["flag"]["colors"];
    assert_eq!(colors["background"], serde_json::json!([1.0, 0.0, 0.0]));
    assert_eq!(colors["accent1"], serde_json::json!([0.0, 1.0, 0.0]));
    assert_eq!(colors["accent2"], serde_json::json!([0.0, 0.0, 1.0]));
}

#[cfg(unix)]
#[test]
fn symlinked_project_manifest_keeps_the_same_scene_on_handoff() {
    let directory = tempfile::tempdir().unwrap();
    let selected = directory.path().join("selected");
    let owner = directory.path().join("owner");
    scene_fixture(&selected);
    scene_fixture(&owner);
    for name in ["project.json", "wallpaper.json"] {
        let manifest = owner.join(name);
        fs::write(&manifest, r#"{"type":"scene","file":"scene.json"}"#).unwrap();
        let link = selected.join("project.json");
        std::os::unix::fs::symlink(&manifest, &link).unwrap();
        let initial = wallbreak::project::resolve(&selected, false).unwrap();
        let handed_off = wallbreak::project::resolve(&initial.source_path, false).unwrap();
        assert_eq!(initial.source_path, manifest);
        assert_eq!(initial.path, owner.join("scene.json"));
        assert_eq!(initial.path, handed_off.path);
        assert_eq!(initial.project_path, handed_off.project_path);
        let output = Command::new(BINARY)
            .arg("inspect")
            .arg(&selected)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(result["path"], owner.join("scene.json").to_str().unwrap());
        let mismatch =
            wallbreak::project::resolve(&selected.join("scene.json"), false).unwrap_err();
        assert!(format!("{mismatch:#}").contains("selected scene.json"));
        fs::remove_file(link).unwrap();
        fs::remove_file(manifest).unwrap();
    }
    let nested = owner.join("nested");
    fs::create_dir(&nested).unwrap();
    let scene = nested.join("scene.json");
    fs::rename(owner.join("scene.json"), &scene).unwrap();
    let manifest = owner.join("project.json");
    fs::write(
        &manifest,
        r#"{"type":"scene","file":"nested/scene.json","preview":"preview.png"}"#,
    )
    .unwrap();
    RgbaImage::new(2, 2)
        .save(owner.join("preview.png"))
        .unwrap();
    std::os::unix::fs::symlink(&manifest, nested.join("project.json")).unwrap();
    let direct = wallbreak::project::resolve(&scene, false).unwrap();
    assert_eq!(direct.path, scene);
    assert_eq!(direct.source_path, scene);
    assert_eq!(direct.project_path, Some(manifest));
    assert_eq!(direct.declared_file.as_deref(), Some("nested/scene.json"));
    let preview = wallbreak::project::resolve(&scene, true).unwrap();
    assert_eq!(preview.path, owner.join("preview.png"));
}

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
fn packed_scene_materializes_into_the_cache_before_checking() {
    let directory = tempfile::tempdir().unwrap();
    scene_fixture(directory.path());
    let names = [
        "scene.json",
        "models/quad.json",
        "materials/quad.json",
        "materials/pixel.tex",
    ];
    let mut table = Vec::new();
    let mut data = Vec::new();
    table.extend_from_slice(&(8u32).to_le_bytes());
    table.extend_from_slice(b"PKGV0018");
    table.extend_from_slice(&(names.len() as u32).to_le_bytes());
    for name in names {
        let payload = fs::read(directory.path().join(name)).unwrap();
        table.extend_from_slice(&(name.len() as u32).to_le_bytes());
        table.extend_from_slice(name.as_bytes());
        table.extend_from_slice(&(data.len() as u32).to_le_bytes());
        table.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        data.extend_from_slice(&payload);
    }
    table.extend_from_slice(&data);
    fs::write(directory.path().join("scene.pkg"), table).unwrap();
    for name in names {
        fs::remove_file(directory.path().join(name)).unwrap();
    }
    fs::write(
        directory.path().join("project.json"),
        r#"{"file":"scene.json","preview":"preview.png","type":"scene"}"#,
    )
    .unwrap();
    RgbaImage::new(2, 2)
        .save(directory.path().join("preview.png"))
        .unwrap();
    let cache = directory.path().join("cache");
    let output = Command::new(BINARY)
        .arg("check")
        .arg(directory.path())
        .args(["--width", "8", "--height", "8", "--frames", "1"])
        .env("XDG_CACHE_HOME", &cache)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["decoded_frames"], 1);
    let selected = result["media"]["path"].as_str().unwrap();
    assert!(
        selected.starts_with(cache.to_str().unwrap()),
        "{selected} is not inside the cache"
    );
    assert!(selected.ends_with("scene.json"));
    assert!(!directory.path().join("scene.json").exists());
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
    assert!(
        String::from_utf8_lossy(&rejected.stderr).contains("no renderable image objects"),
        "{}",
        String::from_utf8_lossy(&rejected.stderr)
    );
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
