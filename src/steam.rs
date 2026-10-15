//! Read-only discovery of installed Wallpaper Engine projects.
//!
//! Scans local Steam libraries for the Wallpaper Engine app (431960) and lists
//! its workshop items and bundled projects. Only public `project.json`
//! declarations (title, type, file, preview) are read; no account data, no
//! workshop metadata, and nothing is executed or modified.

use std::{
    collections::HashSet,
    fs::File,
    io::Read,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use serde::Serialize;

use crate::scene;

/// Wallpaper Engine's Steam app ID.
pub const WALLPAPER_ENGINE_APP_ID: &str = "431960";

const MAX_JSON_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Clone, Serialize)]
pub struct ProjectEntry {
    /// Workshop item ID, or `bundled:<name>` for shipped projects.
    pub id: String,
    pub title: String,
    /// Declared project type: `scene`, `video`, `web`, or `unknown`.
    pub kind: String,
    /// Absolute project directory.
    pub path: PathBuf,
    pub preview: Option<PathBuf>,
    /// Whether wallbreak can play this project now.
    pub supported: bool,
    pub support_note: String,
}

#[derive(Debug, Clone, PartialEq)]
enum Vdf {
    Str(String),
    Obj(Vec<(String, Vdf)>),
}

type VdfEntries = Vec<(String, Vdf)>;
type VdfFrame = (Option<String>, VdfEntries);

impl Vdf {
    fn get(&self, key: &str) -> Option<&Vdf> {
        if let Vdf::Obj(entries) = self {
            entries
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value)
        } else {
            None
        }
    }
}

fn tokenize(text: &str) -> Result<Vec<String>> {
    let mut tokens = Vec::new();
    let mut chars = text.chars().peekable();
    while let Some(&chr) = chars.peek() {
        if chr.is_whitespace() {
            chars.next();
        } else if chr == '{' || chr == '}' {
            tokens.push(chr.to_string());
            chars.next();
        } else if chr == '"' {
            chars.next();
            let mut value = String::new();
            loop {
                match chars.next() {
                    Some('"') => break,
                    Some('\\') => {
                        if let Some(escaped) = chars.next() {
                            value.push(escaped);
                        }
                    }
                    Some(other) => value.push(other),
                    None => anyhow::bail!("unterminated string in VDF document"),
                }
            }
            tokens.push(format!("\"{value}\""));
        } else {
            anyhow::bail!("unexpected character {chr:?} in VDF document");
        }
    }
    Ok(tokens)
}

fn parse_vdf(text: &str) -> Result<Vdf> {
    let tokens = tokenize(text)?;
    // Each frame is the key owning it plus its entries. The root frame has no key.
    let mut stack: Vec<VdfFrame> = vec![(None, Vec::new())];
    let mut pending: Option<String> = None;
    for token in tokens {
        if token == "{" {
            let key = pending
                .take()
                .context("object without a key in VDF document")?;
            stack.push((Some(key), Vec::new()));
        } else if token == "}" {
            let (key, entries) = stack.pop().context("unbalanced VDF document")?;
            let key = key.context("unbalanced VDF document")?;
            let parent = stack.last_mut().context("unbalanced VDF document")?;
            parent.1.push((key, Vdf::Obj(entries)));
        } else {
            let value = token
                .strip_prefix('"')
                .and_then(|inner| inner.strip_suffix('"'))
                .context("invalid VDF token")?
                .to_string();
            if let Some(key) = pending.take() {
                let parent = stack.last_mut().context("unbalanced VDF document")?;
                parent.1.push((key, Vdf::Str(value)));
            } else {
                pending = Some(value);
            }
        }
    }
    if stack.len() != 1 || pending.is_some() {
        anyhow::bail!("unbalanced VDF document");
    }
    Ok(Vdf::Obj(stack.pop().unwrap_or((None, Vec::new())).1))
}

/// Library paths declared by one `libraryfolders.vdf` document.
fn library_paths(document: &Vdf) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    // Modern nested format: "libraryfolders" -> "0" -> { "path": ... }.
    if let Some(libraries) = document.get("libraryfolders") {
        if let Vdf::Obj(entries) = libraries {
            for (_, library) in entries {
                if let Some(Vdf::Str(path)) = library.get("path") {
                    paths.push(PathBuf::from(path));
                }
            }
        }
        return paths;
    }
    // Legacy format: "LibraryFolders" -> "1" -> "/path".
    if let Vdf::Obj(entries) = document {
        if let Some(Vdf::Obj(entries)) = entries
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("libraryfolders"))
            .map(|(_, value)| value)
        {
            for (name, value) in entries {
                if name.chars().all(|chr| chr.is_ascii_digit()) {
                    if let Vdf::Str(path) = value {
                        paths.push(PathBuf::from(path));
                    }
                }
            }
        }
    }
    paths
}

fn read_json(path: &Path) -> Result<serde_json::Value> {
    let mut bytes = Vec::new();
    File::open(path)
        .with_context(|| format!("cannot open {}", path.display()))?
        .take(MAX_JSON_BYTES + 1)
        .read_to_end(&mut bytes)?;
    anyhow::ensure!(
        bytes.len() as u64 <= MAX_JSON_BYTES,
        "{} exceeds the JSON budget",
        path.display()
    );
    serde_json::from_slice(&bytes).with_context(|| format!("invalid {}", path.display()))
}

/// Candidate Steam roots on this machine, deduplicated best-effort.
pub fn default_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    let mut seen = HashSet::new();
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        for candidate in [
            home.join(".local/share/Steam"),
            home.join(".steam/root"),
            home.join(".var/app/com.valvesoftware.Steam/.local/share/Steam"),
        ] {
            let normalized = candidate.canonicalize().unwrap_or(candidate);
            if seen.insert(normalized.clone()) && normalized.is_dir() {
                roots.push(normalized);
            }
        }
    }
    roots
}

/// Library directories from every `libraryfolders.vdf` under the given roots.
pub fn libraries(roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut libraries = Vec::new();
    let mut seen = HashSet::new();
    for root in roots {
        // The root itself is usually a library.
        if root.join("steamapps").is_dir() && seen.insert(root.clone()) {
            libraries.push(root.clone());
        }
        for manifest in [
            root.join("steamapps/libraryfolders.vdf"),
            root.join("config/libraryfolders.vdf"),
        ] {
            let Ok(text) = std::fs::read_to_string(&manifest) else {
                continue;
            };
            let Ok(document) = parse_vdf(&text) else {
                continue;
            };
            for path in library_paths(&document) {
                if path.join("steamapps").is_dir() && seen.insert(path.clone()) {
                    libraries.push(path);
                }
            }
        }
    }
    libraries
}

/// Wallpaper Engine installation directory inside one library, if present.
pub fn installation(library: &Path) -> Option<PathBuf> {
    let manifest = library.join(format!(
        "steamapps/appmanifest_{WALLPAPER_ENGINE_APP_ID}.acf"
    ));
    if !manifest.is_file() {
        return None;
    }
    let install = library.join("steamapps/common/wallpaper_engine");
    install.is_dir().then_some(install)
}

fn support_for(project_dir: &Path, manifest: &serde_json::Value) -> (bool, String) {
    let kind = manifest
        .get("type")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown");
    let file = manifest
        .get("file")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    match kind {
        "video" if !file.is_empty() => (
            true,
            "Video project; FFmpeg validates the actual media on play.".to_string(),
        ),
        "scene" if !file.is_empty() => {
            let properties = manifest
                .get("general")
                .and_then(|general| general.get("properties"))
                .and_then(|properties| properties.as_object());
            let Some(properties) = properties else {
                return (false, "Scene project without color properties.".to_string());
            };
            let colors: std::collections::HashMap<String, serde_json::Value> = properties
                .iter()
                .map(|(name, value)| (name.clone(), value.clone()))
                .collect();
            let Ok(colors) = scene::parse_project_colors(&colors) else {
                return (
                    false,
                    "Scene project without the three flag tint colors.".to_string(),
                );
            };
            let scene_path = project_dir.join(file);
            match scene::parse(&scene_path, project_dir, &colors) {
                Ok(_) => (true, "Supported animated flag scene.".to_string()),
                Err(error) => (
                    false,
                    format!(
                        "Unsupported scene: {:#}",
                        error
                            .to_string()
                            .split(':')
                            .next()
                            .unwrap_or("unknown feature")
                    ),
                ),
            }
        }
        "web" => (false, "Web wallpapers are not supported.".to_string()),
        _ => (false, "Unknown or unsupported project type.".to_string()),
    }
}

fn entry(id: String, project_dir: &Path) -> Option<ProjectEntry> {
    let manifest_path = project_dir.join("project.json");
    let manifest = read_json(&manifest_path).ok()?;
    let title = manifest
        .get("title")
        .and_then(serde_json::Value::as_str)
        .unwrap_or(&id)
        .to_string();
    let kind = manifest
        .get("type")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    let preview = manifest
        .get("preview")
        .and_then(serde_json::Value::as_str)
        .map(|preview| project_dir.join(preview))
        .filter(|path| path.is_file());
    let (supported, support_note) = support_for(project_dir, &manifest);
    Some(ProjectEntry {
        id,
        title,
        kind,
        path: project_dir.to_path_buf(),
        preview,
        supported,
        support_note,
    })
}

/// All Wallpaper Engine projects: workshop items plus bundled projects.
pub fn scan(roots: &[PathBuf]) -> Vec<ProjectEntry> {
    let mut entries = Vec::new();
    let mut seen = HashSet::new();
    for library in libraries(roots) {
        let Some(install) = installation(&library) else {
            continue;
        };
        // Workshop subscriptions.
        let workshop = library.join(format!(
            "steamapps/workshop/content/{WALLPAPER_ENGINE_APP_ID}"
        ));
        if let Ok(items) = std::fs::read_dir(&workshop) {
            for item in items.flatten() {
                let dir = item.path();
                if !dir.is_dir() {
                    continue;
                }
                let id = item.file_name().to_string_lossy().into_owned();
                if seen.insert(dir.clone()) {
                    if let Some(entry) = entry(id, &dir) {
                        entries.push(entry);
                    }
                }
            }
        }
        // Bundled and user projects shipped with the installation.
        let projects = install.join("projects");
        if let Ok(collections) = std::fs::read_dir(&projects) {
            for collection in collections.flatten() {
                let collection = collection.path();
                if !collection.is_dir() {
                    continue;
                }
                let Ok(members) = std::fs::read_dir(&collection) else {
                    continue;
                };
                for member in members.flatten() {
                    let dir = member.path();
                    if !dir.is_dir() || !seen.insert(dir.clone()) {
                        continue;
                    }
                    let name = member.file_name().to_string_lossy().into_owned();
                    if let Some(entry) = entry(format!("bundled:{name}"), &dir) {
                        entries.push(entry);
                    }
                }
            }
        }
    }
    entries.sort_by_key(|a| a.title.to_lowercase());
    entries
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_modern_and_legacy_library_folders() {
        let modern = parse_vdf(
            r#""libraryfolders"
{
	"0"
	{
		"path"		"/games/steam"
		"apps"
		{
			"431960"		"826275581"
		}
	}
}"#,
        )
        .unwrap();
        assert_eq!(library_paths(&modern), [PathBuf::from("/games/steam")]);
        let legacy = parse_vdf(
            r#""LibraryFolders"
{
	"TimeNextStatsReport"		"123"
	"1"		"/old/steam"
}"#,
        )
        .unwrap();
        assert_eq!(library_paths(&legacy), [PathBuf::from("/old/steam")]);
    }

    #[test]
    fn rejects_malformed_vdf_without_panicking() {
        // An empty document parses as an empty object.
        assert!(parse_vdf("").unwrap() == Vdf::Obj(Vec::new()));
        for document in [
            "\"unclosed\"",
            "\"key\" \"value\" \"stray\"",
            "\"a\" { \"b\" ",
            "\"a\" }",
            "bare",
        ] {
            assert!(parse_vdf(document).is_err(), "{document:?}");
        }
    }

    fn layout(root: &Path) -> PathBuf {
        let library = root.join("lib");
        let steamapps = library.join("steamapps");
        std::fs::create_dir_all(steamapps.join("common/wallpaper_engine/projects/bundled/demo"))
            .unwrap();
        std::fs::create_dir_all(steamapps.join("workshop/content/431960/777")).unwrap();
        std::fs::write(steamapps.join("appmanifest_431960.acf"), "\"AppState\"{}").unwrap();
        let text = format!(
            "\"libraryfolders\"\n{{\n\t\"0\"\n\t{{\n\t\t\"path\"\t\t\"{}\"\n\t}}\n}}",
            library.display()
        );
        std::fs::write(steamapps.join("libraryfolders.vdf"), text).unwrap();
        std::fs::write(
            steamapps.join("common/wallpaper_engine/projects/bundled/demo/project.json"),
            r#"{"title":"Demo Clip","type":"video","file":"clip.mp4"}"#,
        )
        .unwrap();
        std::fs::write(
            steamapps.join("workshop/content/431960/777/project.json"),
            r#"{"title":"Workshop Page","type":"web","file":"index.html"}"#,
        )
        .unwrap();
        library
    }

    #[test]
    fn discovers_workshop_and_bundled_projects() {
        let root = tempfile::tempdir().unwrap();
        let library = layout(root.path());
        let entries = scan(&[library]);
        // The synthetic root itself is not a library (no steamapps dir).
        assert_eq!(entries.len(), 2);
        let workshop = entries.iter().find(|entry| entry.id == "777").unwrap();
        assert_eq!(workshop.title, "Workshop Page");
        assert!(!workshop.supported);
        let bundled = entries
            .iter()
            .find(|entry| entry.id == "bundled:demo")
            .unwrap();
        assert!(bundled.supported);
        assert_eq!(bundled.kind, "video");
    }

    #[test]
    fn marks_supported_flag_scenes_without_decoding_textures() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        std::fs::create_dir_all(project.join("models")).unwrap();
        std::fs::create_dir_all(project.join("materials")).unwrap();
        std::fs::write(
            project.join("project.json"),
            r#"{"title":"Flag","type":"scene","file":"scene.json","general":{"properties":{"schemecolor":{"type":"color","value":"0 0 0"},"flagcolor1":{"type":"color","value":"1 1 1"},"flagcolor2":{"type":"color","value":"1 0 0"}}}}"#,
        )
        .unwrap();
        std::fs::write(
            project.join("scene.json"),
            r#"{"camera":{"center":"0 0 -1","eye":"0 0 0","up":"0 1 0"},"general":{"bloom":false,"clearcolor":"0 0 0","orthogonalprojection":{"width":4,"height":2}},"objects":[{"angles":"0 0 0","id":1,"image":"models/quad.json","name":"quad","origin":"2 1 0","scale":"1 1 1"}]}"#,
        )
        .unwrap();
        std::fs::write(
            project.join("models/quad.json"),
            r#"{"height":2,"material":"materials/flag.json","width":4}"#,
        )
        .unwrap();
        std::fs::write(
            project.join("materials/flag.json"),
            r#"{"passes":[{"combos":{"TINT":1},"constantshadervalues":{"Speed":0.5,"Strength":0.5},"depthtest":"disabled","depthwrite":"disabled","shader":"flag","textures":["a","b","c"],"usershadervalues":{"flagcolor1":"color2","flagcolor2":"color3","schemecolor":"color1"}}]}"#,
        )
        .unwrap();
        // Declared texture files must exist; their bytes are never decoded here.
        for stem in ["a", "b", "c"] {
            std::fs::write(project.join(format!("materials/{stem}.tex")), b"").unwrap();
        }
        let manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(project.join("project.json")).unwrap()).unwrap();
        let (supported, _) = support_for(&project, &manifest);
        // Texture files are absent, yet structural support is detected.
        assert!(supported);
    }
}
