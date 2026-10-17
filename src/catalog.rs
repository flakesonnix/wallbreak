use std::{
    collections::HashSet,
    fs,
    io::ErrorKind,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
};

use anyhow::{Context, Result, ensure};

use crate::{
    project::{self, MediaKind, file_kind},
    scene,
    steam::{self, ProjectEntry},
};

const MAX_WARNINGS: usize = 100;

#[derive(Debug)]
pub struct ScanResult {
    pub entries: Vec<ProjectEntry>,
    pub warnings: Vec<String>,
}

pub fn scan(
    steam_roots: &[PathBuf],
    folders: &[PathBuf],
    cancel: &AtomicBool,
) -> Result<ScanResult> {
    let check_cancel = || -> Result<()> {
        ensure!(!cancel.load(Ordering::Relaxed), "catalog scan cancelled");
        Ok(())
    };
    check_cancel()?;

    let mut entries = Vec::new();
    let mut seen = HashSet::new();
    let mut add_entry = |entry: ProjectEntry| {
        if seen.insert(entry.path.clone()) {
            entries.push(entry);
        }
    };
    let mut warnings = Vec::new();
    let mut omitted_warnings = 0usize;
    let mut warn = |warning: String| {
        if warnings.len() < MAX_WARNINGS {
            warnings.push(warning);
        } else {
            omitted_warnings += 1;
        }
    };

    for mut entry in steam::scan_cancellable(steam_roots, cancel)? {
        check_cancel()?;
        match entry.path.canonicalize() {
            Ok(path) => {
                entry.path = path;
                add_entry(entry);
            }
            Err(error) => warn(format!(
                "cannot resolve Steam project {}: {error}",
                entry.path.display()
            )),
        }
    }
    check_cancel()?;

    let mut scanned_folders = HashSet::new();
    for folder in folders {
        check_cancel()?;
        let folder = match folder.canonicalize() {
            Ok(path) => path,
            Err(error) => {
                warn(format!(
                    "cannot resolve folder {}: {error}",
                    folder.display()
                ));
                continue;
            }
        };
        if !folder.is_dir() {
            warn(format!("not a directory: {}", folder.display()));
            continue;
        }
        if !scanned_folders.insert(folder.clone()) {
            continue;
        }
        match local_entry(&folder) {
            Ok(Some(entry)) => {
                add_entry(entry);
                continue;
            }
            Ok(None) => {}
            Err(error) => {
                warn(format!("{error:#}"));
                continue;
            }
        }

        let children = match fs::read_dir(&folder) {
            Ok(children) => children,
            Err(error) => {
                warn(format!("cannot read folder {}: {error}", folder.display()));
                continue;
            }
        };
        for child in children {
            check_cancel()?;
            let child = match child {
                Ok(child) => child,
                Err(error) => {
                    warn(format!(
                        "cannot read entry in {}: {error}",
                        folder.display()
                    ));
                    continue;
                }
            };
            let path = child.path();
            match child.file_type() {
                Ok(kind) if kind.is_symlink() && !path.is_file() => continue,
                Ok(_) => {}
                Err(error) => {
                    warn(format!("cannot inspect {}: {error}", path.display()));
                    continue;
                }
            }
            match local_entry(&path) {
                Ok(Some(entry)) => add_entry(entry),
                Ok(None) => {}
                Err(error) => warn(format!("{error:#}")),
            }
        }
    }
    check_cancel()?;
    if omitted_warnings > 0 {
        warnings.push(format!(
            "{omitted_warnings} additional catalog warnings omitted."
        ));
    }
    entries.sort_by(|a, b| {
        a.title
            .to_lowercase()
            .cmp(&b.title.to_lowercase())
            .then_with(|| a.path.cmp(&b.path))
    });
    check_cancel()?;
    Ok(ScanResult { entries, warnings })
}

fn local_entry(path: &Path) -> Result<Option<ProjectEntry>> {
    let mut path = path
        .canonicalize()
        .with_context(|| format!("cannot resolve {}", path.display()))?;
    if path.is_dir() {
        let manifest = path.join("project.json");
        match fs::symlink_metadata(&manifest) {
            Ok(_) => {
                return steam::project_entry(format!("local:{}", path.display()), &path)
                    .map(Some)
                    .with_context(|| {
                        format!(
                            "invalid or unreadable project manifest {}",
                            manifest.display()
                        )
                    });
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("cannot inspect {}", manifest.display()));
            }
        }
        let scene_path = path.join("scene.json");
        match fs::symlink_metadata(&scene_path) {
            Ok(_) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("cannot inspect {}", scene_path.display()));
            }
        }
        path = scene_path
            .canonicalize()
            .with_context(|| format!("cannot resolve {}", scene_path.display()))?;
        ensure!(
            path.is_file(),
            "{} is not a regular scene file",
            scene_path.display()
        );
    }
    if !path.is_file() {
        return Ok(None);
    }
    if file_kind(&path) == Some(MediaKind::Scene) {
        let source_root = path
            .parent()
            .context("scene.json has no parent directory")?;
        let attached = project::scene_project(&path)
            .with_context(|| format!("invalid adjacent project for {}", path.display()))?;
        let root = match &attached {
            Some((manifest_path, _)) => manifest_path
                .parent()
                .context("project manifest has no parent directory")?,
            None => source_root,
        };
        let manifest = attached.as_ref().map(|(_, manifest)| manifest);
        let title = manifest
            .and_then(|manifest| manifest.title.clone())
            .unwrap_or_else(|| {
                root.file_name()
                    .unwrap_or(root.as_os_str())
                    .to_string_lossy()
                    .into_owned()
            });
        let preview = manifest
            .and_then(|manifest| manifest.preview.as_deref())
            .and_then(|preview| project::project_file(root, preview).ok())
            .filter(|preview| file_kind(preview) == Some(MediaKind::Image));
        let parsed = manifest
            .map(project::Project::colors)
            .transpose()
            .and_then(|colors| scene::parse(&path, root, &colors.unwrap_or_default()));
        let (supported, support_note) = match parsed {
            Ok(_) => (
                true,
                "Supported scene structure; textures are decoded and validated on play."
                    .to_string(),
            ),
            Err(error) => (false, format!("Unsupported scene: {error:#}")),
        };
        return Ok(Some(ProjectEntry {
            id: format!("local:{}", path.display()),
            title,
            kind: "scene".into(),
            path,
            preview,
            supported,
            support_note,
        }));
    }
    let (kind, preview, support_note) = match file_kind(&path) {
        Some(MediaKind::Image) => (
            "image",
            Some(path.clone()),
            "Image selected by extension; decoded as a single static frame on play. Animation and EXIF rotation are not applied; this scan does not validate decodability.",
        ),
        Some(MediaKind::Video) => (
            "video",
            None,
            "Video selected by extension; FFmpeg validates actual decodability on play. Playback is muted, software-decoded, and looped.",
        ),
        _ => return Ok(None),
    };
    Ok(Some(ProjectEntry {
        id: format!("local:{}", path.display()),
        title: path
            .file_name()
            .unwrap_or(path.as_os_str())
            .to_string_lossy()
            .into_owned(),
        kind: kind.into(),
        path,
        preview,
        supported: true,
        support_note: support_note.into(),
    }))
}
