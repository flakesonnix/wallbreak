use std::{
    collections::HashMap,
    fs::{self, File},
    io::Read,
    path::{Component, Path, PathBuf},
};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};

use crate::{
    pkg,
    scene::{self, Scene, SceneProfile},
};

const MAX_PROJECT_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaKind {
    Image,
    Video,
    Scene,
}

#[derive(Debug, Clone, Serialize)]
pub struct Media {
    pub path: PathBuf,
    /// The source file to re-resolve, distinct from its selected payload or preview.
    #[serde(skip_serializing)]
    pub source_path: PathBuf,
    pub kind: MediaKind,
    pub project_path: Option<PathBuf>,
    pub declared_file: Option<String>,
    pub preview_only: bool,
    #[serde(skip_serializing)]
    pub scene: Option<Scene>,
    pub scene_profile: Option<SceneProfile>,
    pub limitations: Vec<String>,
}

#[derive(Deserialize)]
pub(crate) struct Project {
    #[serde(skip)]
    pub path: PathBuf,
    pub title: Option<String>,
    #[serde(rename = "type")]
    pub kind: Option<String>,
    pub file: Option<String>,
    pub preview: Option<String>,
    general: Option<ProjectGeneral>,
}

#[derive(Deserialize)]
struct ProjectGeneral {
    #[serde(default)]
    properties: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProjectKind {
    Scene,
    Video,
    Image,
    Web,
    Application,
}

impl ProjectKind {
    /// Wallpaper Engine writes type names with inconsistent casing ("Scene").
    pub(crate) fn from_declaration(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "scene" => Some(Self::Scene),
            "video" => Some(Self::Video),
            "image" | "picture" | "img" => Some(Self::Image),
            "web" | "website" => Some(Self::Web),
            "application" | "app" | "program" => Some(Self::Application),
            _ => None,
        }
    }

    /// Wallpaper Engine 1.x projects may omit `type`; the declared file decides.
    pub(crate) fn infer_from_file(file: &str) -> Option<Self> {
        if let Some(kind) = file_kind(Path::new(file)) {
            return Some(match kind {
                MediaKind::Scene => Self::Scene,
                MediaKind::Video => Self::Video,
                MediaKind::Image => Self::Image,
            });
        }
        match Path::new(file)
            .extension()
            .and_then(|extension| extension.to_str())
            .map(|extension| extension.to_ascii_lowercase())
            .as_deref()
        {
            Some("json") => Some(Self::Scene),
            Some("html") | Some("htm") => Some(Self::Web),
            Some("exe") => Some(Self::Application),
            _ => None,
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Scene => "scene",
            Self::Video => "video",
            Self::Image => "image",
            Self::Web => "web",
            Self::Application => "application",
        }
    }
}

impl Project {
    /// Declared type when present and recognized, otherwise inferred from the
    /// declared file. Projects with neither stay unknown.
    pub(crate) fn resolved_kind(&self) -> Option<ProjectKind> {
        self.kind
            .as_deref()
            .and_then(ProjectKind::from_declaration)
            .or_else(|| self.file.as_deref().and_then(ProjectKind::infer_from_file))
    }

    /// Human-facing kind for listings: the declaration as written, the inferred
    /// kind, or `unknown`.
    pub(crate) fn listing_kind(&self) -> String {
        match &self.kind {
            Some(kind) => kind.clone(),
            None => self
                .resolved_kind()
                .map(|kind| kind.as_str().to_string())
                .unwrap_or_else(|| "unknown".into()),
        }
    }
}

impl Project {
    pub(crate) fn read(path: &Path) -> Result<Self> {
        let path = path
            .canonicalize()
            .with_context(|| format!("cannot resolve project manifest {}", path.display()))?;
        let mut bytes = Vec::new();
        open_regular_file(&path)?
            .take(MAX_PROJECT_BYTES + 1)
            .read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() as u64 <= MAX_PROJECT_BYTES,
            "project.json exceeds 1 MiB"
        );
        let value: serde_json::Value =
            serde_json::from_slice(&bytes).context("invalid project.json")?;
        ensure!(value.is_object(), "project.json must be a JSON object");
        let mut project: Self = serde_json::from_value(value).context("invalid project.json")?;
        project.path = path;
        Ok(project)
    }

    pub(crate) fn colors(&self) -> Result<HashMap<String, [f32; 3]>> {
        match &self.general {
            Some(general) => scene::parse_project_colors(&general.properties),
            None => Ok(HashMap::new()),
        }
    }
}

/// Attach only the adjacent manifest that actually declares this scene. A present
/// but invalid or mismatched manifest must not be treated as an absent project.
pub(crate) fn scene_project(scene_path: &Path) -> Result<Option<(PathBuf, Project)>> {
    let root = scene_path
        .parent()
        .context("scene.json has no parent directory")?;
    let path = root.join("project.json");
    match fs::symlink_metadata(&path) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("cannot inspect scene project manifest"),
    }
    let project = Project::read(&path)?;
    let root = project
        .path
        .parent()
        .context("project manifest has no parent directory")?;
    ensure!(
        project.resolved_kind() == Some(ProjectKind::Scene),
        "scene.json requires its adjacent project.json to declare a scene project"
    );
    let declaration = project
        .file
        .as_deref()
        .context("scene project has no file declaration")?;
    ensure!(
        project_file(root, declaration)? == scene_path,
        "project.json does not declare the selected scene.json; refusing to select a different scene"
    );
    Ok(Some((project.path.clone(), project)))
}

/// Select a declared scene file, materializing an adjacent `<stem>.pkg` package
/// when the declared file is absent. Returns the scene file and the root that
/// its relative references resolve against.
pub(crate) fn scene_selection(
    project_root: &Path,
    declaration: &str,
) -> Result<(PathBuf, PathBuf)> {
    match pkg::scene_source(project_root, declaration)? {
        Some(selection) => Ok(selection),
        None => Ok((
            project_file(project_root, declaration)?,
            project_root.to_path_buf(),
        )),
    }
}

pub fn resolve(input: &Path, preview: bool) -> Result<Media> {
    let input = fs::canonicalize(input)
        .with_context(|| format!("cannot resolve input {}", input.display()))?;
    let direct_scene = !input.is_dir() && file_kind(&input) == Some(MediaKind::Scene);
    let attached = if direct_scene {
        scene_project(&input)?
    } else if input.is_dir()
        || input
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("json"))
    {
        let path = if input.is_dir() {
            input.join("project.json")
        } else {
            input.clone()
        };
        let project = Project::read(&path)?;
        Some((project.path.clone(), project))
    } else {
        ensure!(
            !preview,
            "--preview requires a project directory, project.json or a project-backed scene.json"
        );
        return media(input.clone(), input, None, None, false);
    };
    let (project_path, project) = attached.unzip();
    let source_path = if direct_scene {
        input.clone()
    } else {
        project_path.clone().context("project source is missing")?
    };
    let root = project_path
        .as_deref()
        .unwrap_or(&source_path)
        .parent()
        .context("source has no parent directory")?;
    let declared_file = if preview {
        Some(
            project
                .as_ref()
                .context("standalone scene.json has no declared project preview")?
                .preview
                .clone()
                .context("project has no preview declaration")?,
        )
    } else if let Some(project) = &project {
        match project.resolved_kind() {
            Some(ProjectKind::Scene) | Some(ProjectKind::Video) | Some(ProjectKind::Image) => Some(
                project
                    .file
                    .clone()
                    .context("project has no file declaration")?,
            ),
            Some(ProjectKind::Web) => bail!(
                "project is a web wallpaper: rendering its page requires a browser engine; use --preview for the static preview"
            ),
            Some(ProjectKind::Application) => bail!(
                "project is an application wallpaper: executing wallpaper binaries is not supported; use --preview for the static preview"
            ),
            None => match project.kind.as_deref() {
                Some(declared) => bail!(
                    "project declares unrecognized type {declared:?}; supported types are scene, video and image"
                ),
                None => bail!(
                    "project type is unknown: project.json declares no recognized type and no file extension to infer from; use an explicit media file or --preview"
                ),
            },
        }
    } else {
        None
    };
    let mut scene_root = root.to_path_buf();
    let selected = if direct_scene && !preview {
        input.clone()
    } else {
        match &declared_file {
            Some(declaration) => {
                let scene_declared = !preview
                    && project
                        .as_ref()
                        .is_some_and(|project| project.resolved_kind() == Some(ProjectKind::Scene));
                if scene_declared {
                    let (scene_path, materialized_root) = scene_selection(root, declaration)?;
                    scene_root = materialized_root;
                    scene_path
                } else {
                    project_file(root, declaration)?
                }
            }
            None => input.clone(),
        }
    };
    if !preview
        && (direct_scene
            || project
                .as_ref()
                .is_some_and(|project| project.resolved_kind() == Some(ProjectKind::Scene)))
    {
        let colors = project
            .as_ref()
            .map(Project::colors)
            .transpose()?
            .unwrap_or_default();
        let started = std::time::Instant::now();
        let scene = scene::load(&selected, &scene_root, &colors)?;
        let profile = scene.profile().clone();
        crate::log_debug!(
            "scene {} parsed in {:.1}ms: {} layer(s), {} texture(s), {} limitation(s)",
            selected.display(),
            started.elapsed().as_secs_f64() * 1000.0,
            profile.layers.len(),
            scene
                .profile()
                .layers
                .iter()
                .map(|layer| layer.material.texture_count())
                .sum::<usize>(),
            profile.limitations.len()
        );
        for limitation in &profile.limitations {
            crate::log_trace!("scene limitation: {limitation}");
        }
        let limitations = profile.limitations.clone();
        return Ok(Media {
            path: selected,
            source_path,
            kind: MediaKind::Scene,
            project_path,
            declared_file,
            preview_only: false,
            scene: Some(scene),
            scene_profile: Some(profile),
            limitations,
        });
    }
    let resolved = media(selected, source_path, project_path, declared_file, preview)?;
    if preview {
        ensure!(
            resolved.kind == MediaKind::Image,
            "project preview must be a supported image"
        );
    } else {
        ensure!(
            matches!(resolved.kind, MediaKind::Video | MediaKind::Image),
            "project must select a supported image or video file"
        );
    }
    Ok(resolved)
}

pub(crate) fn open_regular_file(path: &Path) -> Result<File> {
    use rustix::fs::{Mode, OFlags, open};

    // Nonblocking open lets us inspect and reject FIFOs before any read.
    let file = File::from(
        open(
            path,
            OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NONBLOCK,
            Mode::empty(),
        )
        .with_context(|| format!("cannot open {}", path.display()))?,
    );
    ensure!(
        file.metadata()?.is_file(),
        "{} is not a regular file",
        path.display()
    );
    Ok(file)
}

pub(crate) fn project_file(root: &Path, declaration: &str) -> Result<PathBuf> {
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

pub fn file_kind(path: &Path) -> Option<MediaKind> {
    if path.file_name().is_some_and(|name| name == "scene.json") {
        return Some(MediaKind::Scene);
    }
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match extension.as_str() {
        "jpg" | "jpeg" | "png" | "webp" | "bmp" | "gif" => Some(MediaKind::Image),
        "mp4" | "m4v" | "mkv" | "webm" | "mov" | "avi" | "ogv" | "wmv" | "mpg" | "mpeg" => {
            Some(MediaKind::Video)
        }
        _ => None,
    }
}

fn media(
    path: PathBuf,
    source_path: PathBuf,
    project_path: Option<PathBuf>,
    declared_file: Option<String>,
    preview: bool,
) -> Result<Media> {
    ensure!(path.is_file(), "media must be a regular file");
    let (kind, mut limitations) = match file_kind(&path) {
        Some(MediaKind::Image) => (
            MediaKind::Image,
            vec!["Images are decoded as a single static frame; animation and EXIF rotation are not applied.".into()],
        ),
        Some(MediaKind::Video) => (
            MediaKind::Video,
            vec!["Video kind is selected by extension; FFmpeg validates actual decodability. Playback is muted, software-decoded, and looped.".into()],
        ),
        _ => {
            let extension = path
                .extension()
                .and_then(|value| value.to_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            if extension == "pkg" {
                bail!(
                    "{} is a packed scene.pkg; select its project directory (or project.json) so the package is materialized automatically",
                    path.display()
                );
            }
            bail!("unsupported media extension {extension:?}; select a supported image, video or scene.json, not a packed scene.pkg")
        }
    };
    if preview {
        limitations.push(
            "Only the declared preview is displayed; the scene or web wallpaper is not rendered."
                .into(),
        );
    }
    Ok(Media {
        path,
        source_path,
        kind,
        project_path,
        declared_file,
        preview_only: preview,
        scene: None,
        scene_profile: None,
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
            r#"{"file":"scene.json","preview":"preview.jpg","type":"scene","general":{"properties":{"schemecolor":{"type":"color","value":"0 0 0"},"flagcolor1":{"type":"color","value":"1 1 1"},"flagcolor2":{"type":"color","value":"1 0 0"}}}}"#,
        );
        fs::write(
            dir.path().join("scene.json"),
            r#"{"camera":{"center":"0 0 -1","eye":"0 0 0","up":"0 1 0"},"general":{"bloom":false,"clearcolor":"0 0 0","orthogonalprojection":{"width":2,"height":2}},"objects":[{"angles":"0 0 0","id":1,"image":"models/a.json","name":"a","origin":"1 1 0","scale":"1 1 1"},{"angles":"0 0 0","id":2,"image":"models/b.json","name":"b","origin":"1 1 0","scale":"1 1 1"}]}"#,
        )
        .unwrap();
        fs::write(dir.path().join("preview.jpg"), b"preview").unwrap();
        assert!(
            resolve(dir.path(), false)
                .unwrap_err()
                .to_string()
                .contains("invalid scene model")
        );
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
        // A missing declaration is inferred from the file extension.
        project(dir.path(), r#"{"file":"clip.mp4"}"#);
        let selected = resolve(dir.path(), false).unwrap();
        assert_eq!(selected.kind, MediaKind::Video);
        project(dir.path(), r#"{"file":"mystery.bin"}"#);
        assert!(
            resolve(dir.path(), false)
                .unwrap_err()
                .to_string()
                .contains("unknown")
        );
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
        assert!(
            resolve(dir.path(), false)
                .unwrap_err()
                .to_string()
                .contains("exceeds")
        );
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
        assert!(
            resolve(dir.path(), false)
                .unwrap_err()
                .to_string()
                .contains("outside")
        );
    }
}
