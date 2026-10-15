use std::{
    fs::{self, File},
    io::Read,
    path::{Component, Path, PathBuf},
};

use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};

const MAX_PROJECT_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaKind {
    Image,
    Video,
}

#[derive(Debug, Clone, Serialize)]
pub struct Media {
    pub path: PathBuf,
    pub kind: MediaKind,
    pub project_path: Option<PathBuf>,
    pub declared_file: Option<String>,
    pub preview_only: bool,
    pub limitations: Vec<String>,
}

#[derive(Deserialize)]
struct Project {
    #[serde(rename = "type")]
    kind: Option<String>,
    file: Option<String>,
    preview: Option<String>,
}

pub fn resolve(input: &Path, preview: bool) -> Result<Media> {
    let input = fs::canonicalize(input)
        .with_context(|| format!("cannot resolve input {}", input.display()))?;
    let project_path = if input.is_dir() {
        Some(input.join("project.json"))
    } else if input.file_name().is_some_and(|name| name == "project.json") {
        Some(input.clone())
    } else {
        None
    };
    let Some(project_path) = project_path else {
        ensure!(
            !preview,
            "--preview requires a project directory or project.json"
        );
        return media(input, None, None, false);
    };

    let mut bytes = Vec::new();
    File::open(&project_path)
        .with_context(|| format!("cannot open {}", project_path.display()))?
        .take(MAX_PROJECT_BYTES + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= MAX_PROJECT_BYTES,
        "project.json exceeds 1 MiB"
    );
    let project: Project = serde_json::from_slice(&bytes).context("invalid project.json")?;
    let declared_file = if preview {
        project
            .preview
            .context("project has no preview declaration")?
    } else {
        match project.kind.as_deref() {
            Some("video") => project.file.context("video project has no file declaration")?,
            Some(kind) => bail!(
                "project type {kind:?} is unsupported: this MVP plays images/videos, not scenes, web pages or executables; use --preview explicitly for its static preview"
            ),
            None => bail!("project type is unknown; use an explicit media file or --preview"),
        }
    };
    let root = project_path
        .parent()
        .context("project has no parent directory")?;
    let selected = project_file(root, &declared_file)?;
    let resolved = media(selected, Some(project_path), Some(declared_file), preview)?;
    if preview {
        ensure!(
            resolved.kind == MediaKind::Image,
            "project preview must be a supported image"
        );
    } else {
        ensure!(
            resolved.kind == MediaKind::Video,
            "video project must select a supported video file"
        );
    }
    Ok(resolved)
}

fn project_file(root: &Path, declaration: &str) -> Result<PathBuf> {
    let relative = Path::new(declaration);
    ensure!(
        !declaration.is_empty()
            && !declaration.contains('\\')
            && relative
                .components()
                .all(|item| matches!(item, Component::Normal(_) | Component::CurDir)),
        "project media must be a relative path without parent traversal or backslashes"
    );
    let root = fs::canonicalize(root)?;
    let selected = fs::canonicalize(root.join(relative))
        .with_context(|| format!("cannot resolve project file {declaration:?}"))?;
    ensure!(
        selected.starts_with(&root),
        "project media resolves outside its project directory"
    );
    ensure!(selected.is_file(), "project media is not a regular file");
    Ok(selected)
}

fn media(
    path: PathBuf,
    project_path: Option<PathBuf>,
    declared_file: Option<String>,
    preview: bool,
) -> Result<Media> {
    ensure!(path.is_file(), "media must be a regular file");
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let (kind, mut limitations) = match extension.as_str() {
        "jpg" | "jpeg" | "png" | "webp" | "bmp" | "gif" => (
            MediaKind::Image,
            vec!["Images are decoded as a single static frame; animation and EXIF rotation are not applied.".into()],
        ),
        "mp4" | "m4v" | "mkv" | "webm" | "mov" | "avi" | "ogv" | "wmv" | "mpg" | "mpeg" => (
            MediaKind::Video,
            vec!["Video kind is selected by extension; FFmpeg validates actual decodability. Playback is muted, software-decoded, and looped.".into()],
        ),
        _ => bail!("unsupported media extension {extension:?}; select a supported image or video, not scene.pkg or scene.json"),
    };
    if preview {
        limitations.push(
            "Only the declared preview is displayed; the scene or web wallpaper is not rendered."
                .into(),
        );
    }
    Ok(Media {
        path,
        kind,
        project_path,
        declared_file,
        preview_only: preview,
        limitations,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(directory: &Path, json: &str) {
        fs::write(directory.join("project.json"), json).unwrap();
    }

    #[test]
    fn resolves_direct_media_without_claiming_codec_validation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("clip.MP4");
        fs::write(&path, b"not yet decoded").unwrap();
        let selected = resolve(&path, false).unwrap();
        assert_eq!(selected.kind, MediaKind::Video);
        assert!(!selected.preview_only);
        assert!(selected.limitations[0].contains("FFmpeg validates"));
    }

    #[test]
    fn scene_preview_is_only_used_explicitly() {
        let dir = tempfile::tempdir().unwrap();
        project(
            dir.path(),
            r#"{"file":"scene.json","preview":"preview.jpg","type":"scene"}"#,
        );
        fs::write(dir.path().join("preview.jpg"), b"preview").unwrap();
        assert!(resolve(dir.path(), false)
            .unwrap_err()
            .to_string()
            .contains("unsupported"));
        let selected = resolve(dir.path(), true).unwrap();
        assert!(selected.preview_only);
        assert_eq!(selected.declared_file.as_deref(), Some("preview.jpg"));
        assert_eq!(selected.kind, MediaKind::Image);
    }

    #[test]
    fn preserves_declared_media_and_rejects_unknown_types() {
        let dir = tempfile::tempdir().unwrap();
        project(
            dir.path(),
            r#"{"type":"video","file":"./clip.mp4","extra":true}"#,
        );
        fs::write(dir.path().join("clip.mp4"), b"video").unwrap();
        let selected = resolve(&dir.path().join("project.json"), false).unwrap();
        assert_eq!(selected.declared_file.as_deref(), Some("./clip.mp4"));
        project(dir.path(), r#"{"file":"clip.mp4"}"#);
        assert!(resolve(dir.path(), false)
            .unwrap_err()
            .to_string()
            .contains("unknown"));
        project(dir.path(), r#"{"type":"video","file":"missing.mp4"}"#);
        assert!(resolve(dir.path(), false).is_err());
    }

    #[test]
    fn rejects_traversal_absolute_paths_and_malformed_projects() {
        let dir = tempfile::tempdir().unwrap();
        for declaration in ["../clip.mp4", "/tmp/clip.mp4", "folder\\clip.mp4", ""] {
            project(
                dir.path(),
                &serde_json::json!({"type":"video","file":declaration}).to_string(),
            );
            assert!(resolve(dir.path(), false).is_err());
        }
        project(dir.path(), "{");
        assert!(resolve(dir.path(), false).is_err());
        project(dir.path(), &" ".repeat(MAX_PROJECT_BYTES as usize + 1));
        assert!(resolve(dir.path(), false)
            .unwrap_err()
            .to_string()
            .contains("exceeds"));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_escaping_project() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let video = outside.path().join("clip.mp4");
        fs::write(&video, b"video").unwrap();
        std::os::unix::fs::symlink(video, dir.path().join("clip.mp4")).unwrap();
        project(dir.path(), r#"{"type":"video","file":"clip.mp4"}"#);
        assert!(resolve(dir.path(), false)
            .unwrap_err()
            .to_string()
            .contains("outside"));
    }
}
