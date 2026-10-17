//! Read-only materialization of packed Wallpaper Engine scenes (`scene.pkg`,
//! PKGV containers) into a local cache so the regular scene pipeline can load
//! them unchanged. Nothing is executed and no bytes are modified.

use std::{
    collections::BTreeSet,
    fs,
    hash::{Hash, Hasher},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    time::SystemTime,
};

use anyhow::{Context, Result, ensure};

use crate::project::open_regular_file;

/// Packed scenes above this size are rejected before reading.
const MAX_PKG_BYTES: u64 = 512 * 1024 * 1024;
/// Per-entry cap; TEX files in shipped packages stay well below this.
const MAX_ENTRY_BYTES: usize = 256 * 1024 * 1024;
/// Total extracted payload cap.
const MAX_TOTAL_BYTES: u64 = 512 * 1024 * 1024;
const MAX_ENTRIES: usize = 65_536;
const MAX_NAME_BYTES: usize = 4_096;

struct Entry {
    name: String,
    offset: usize,
    size: usize,
}

/// Materialize the declared scene file of a project root, extracting a `<stem>.pkg`
/// next to `project.json` when the declared file itself is absent.
///
/// Returns the scene file path and the root its references resolve against; for an
/// extracted package both live under the cache directory. `Ok(None)` means no
/// package exists and the caller should use the regular project layout.
pub(crate) fn scene_source(
    project_root: &Path,
    declaration: &str,
) -> Result<Option<(PathBuf, PathBuf)>> {
    let declared = Path::new(declaration);
    ensure!(
        declared
            .extension()
            .is_some_and(|extension| extension == "json"),
        "packed scenes require a declared .json scene file, got {declaration:?}"
    );
    let stem = declared
        .file_stem()
        .and_then(|stem| stem.to_str())
        .context("scene declaration has no usable file stem")?;
    let pkg_path = project_root.join(format!("{stem}.pkg"));
    let metadata = match fs::metadata(&pkg_path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("cannot inspect packed scene {}", pkg_path.display()));
        }
    };
    ensure!(
        metadata.is_file(),
        "packed scene {} is not a regular file",
        pkg_path.display()
    );
    let modified = metadata
        .modified()
        .map(|time| {
            time.duration_since(SystemTime::UNIX_EPOCH)
                .map(|since| since.as_nanos())
                .unwrap_or_default()
        })
        .unwrap_or_default();
    let key = cache_key(project_root, metadata.len(), modified);
    let cache_root = cache_dir()?.join(&key);
    let marker = cache_root.join(".complete");
    let expected = format!("{} {modified}\n", metadata.len());
    if fs::read_to_string(&marker).ok().as_deref() == Some(&expected) {
        let cached = cache_root.join(declaration);
        ensure!(
            cached.is_file(),
            "packed scene cache {} lost its {}",
            cache_root.display(),
            declaration
        );
        crate::log_debug!("reusing packed scene cache {}", cache_root.display());
        return Ok(Some((cached, cache_root)));
    }
    if cache_root.exists() {
        fs::remove_dir_all(&cache_root).with_context(|| {
            format!(
                "cannot refresh stale packed scene cache {}",
                cache_root.display()
            )
        })?;
    }
    let started = std::time::Instant::now();
    let entries = extract_to_cache(&pkg_path, &cache_root)?;
    fs::write(&marker, &expected).context("cannot write packed scene cache marker")?;
    crate::log_debug!(
        "extracted {} files from {} in {:.1}ms to {}",
        entries,
        pkg_path.display(),
        started.elapsed().as_secs_f64() * 1000.0,
        cache_root.display()
    );
    let scene_path = cache_root.join(declaration);
    ensure!(
        scene_path.is_file(),
        "packed scene {} does not contain its declared {declaration}",
        pkg_path.display()
    );
    Ok(Some((scene_path, cache_root)))
}

fn cache_dir() -> Result<PathBuf> {
    if let Some(cache) = std::env::var_os("XDG_CACHE_HOME").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(cache).join("wallbreak").join("pkg"));
    }
    let home =
        std::env::var_os("HOME").context("cannot locate the cache directory: HOME is unset")?;
    Ok(PathBuf::from(home)
        .join(".cache")
        .join("wallbreak")
        .join("pkg"))
}

fn cache_key(project_root: &Path, size: u64, modified: u128) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    project_root.hash(&mut hasher);
    size.hash(&mut hasher);
    modified.hash(&mut hasher);
    let name = project_root
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("scene");
    let safe: String = name
        .chars()
        .filter(|character| {
            character.is_ascii_alphanumeric() || *character == '-' || *character == '_'
        })
        .take(40)
        .collect();
    format!("{safe}-{:016x}", hasher.finish())
}

fn extract_to_cache(pkg_path: &Path, cache_root: &Path) -> Result<usize> {
    let entries = parse(pkg_path)?;
    let mut bytes = Vec::new();
    let mut file = open_regular_file(pkg_path)?;
    file.read_to_end(&mut bytes)
        .context("cannot read packed scene")?;
    let mut total = 0u64;
    let staging = cache_root.with_extension(format!("staging{}", std::process::id()));
    if staging.exists() {
        fs::remove_dir_all(&staging)
            .with_context(|| format!("cannot clear staging directory {}", staging.display()))?;
    }
    let result = (|| -> Result<()> {
        for entry in &entries {
            let end = entry
                .offset
                .checked_add(entry.size)
                .context("packed scene entry overflows")?;
            ensure!(
                end <= bytes.len(),
                "packed scene entry {:?} exceeds the package",
                entry.name
            );
            total = total
                .checked_add(entry.size as u64)
                .context("packed scene total size overflows")?;
            ensure!(
                total <= MAX_TOTAL_BYTES,
                "packed scene payload exceeds its 512 MiB budget"
            );
            let target = staging.join(&entry.name);
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("cannot create {}", parent.display()))?;
            }
            let mut output = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&target)
                .with_context(|| format!("cannot create extracted {}", target.display()))?;
            output
                .write_all(&bytes[entry.offset..end])
                .with_context(|| format!("cannot write extracted {}", target.display()))?;
        }
        Ok(())
    })();
    if let Err(error) = result {
        let _ = fs::remove_dir_all(&staging);
        return Err(error);
    }
    fs::create_dir_all(
        cache_root
            .parent()
            .context("packed scene cache has no parent")?,
    )
    .context("cannot create packed scene cache")?;
    fs::rename(&staging, cache_root).with_context(|| {
        format!(
            "cannot move extracted package into {}",
            cache_root.display()
        )
    })?;
    Ok(entries.len())
}

fn parse(pkg_path: &Path) -> Result<Vec<Entry>> {
    let file = open_regular_file(pkg_path)?;
    let metadata = file.metadata().context("cannot inspect packed scene")?;
    ensure!(
        metadata.len() <= MAX_PKG_BYTES,
        "packed scene exceeds its 512 MiB input budget"
    );
    let mut bytes = Vec::new();
    file.take(MAX_PKG_BYTES + 1)
        .read_to_end(&mut bytes)
        .context("cannot read packed scene")?;
    ensure!(
        bytes.len() as u64 <= MAX_PKG_BYTES,
        "packed scene grew past its input budget"
    );
    let mut cursor = Cursor::new(&bytes);
    let version_len = cursor.u32()? as usize;
    ensure!(
        (4..=16).contains(&version_len),
        "unsupported scene.pkg version length {version_len}"
    );
    let version = cursor.take(version_len)?.to_vec();
    let version = std::str::from_utf8(&version)
        .context("scene.pkg version is not UTF-8")?
        .to_string();
    ensure!(
        version.starts_with("PKGV") && version[4..].bytes().all(|byte| byte.is_ascii_digit()),
        "unsupported scene.pkg magic {version:?}"
    );
    let count = cursor.u32()? as usize;
    ensure!(
        count <= MAX_ENTRIES,
        "scene.pkg declares {count} entries; the limit is {MAX_ENTRIES}"
    );
    let table_start = cursor.position();
    let mut entries = Vec::with_capacity(count.min(1024));
    let mut names = BTreeSet::new();
    for _ in 0..count {
        let name_len = cursor.u32()? as usize;
        ensure!(
            name_len > 0 && name_len <= MAX_NAME_BYTES,
            "scene.pkg entry name length {name_len} is invalid"
        );
        let name = cursor.take(name_len)?.to_vec();
        let name = std::str::from_utf8(&name)
            .context("scene.pkg entry name is not UTF-8")?
            .to_string();
        validate_name(&name)?;
        ensure!(
            names.insert(name.clone()),
            "scene.pkg contains duplicate entry {name:?}"
        );
        let offset = cursor.u32()? as usize;
        let size = cursor.u32()? as usize;
        ensure!(
            size <= MAX_ENTRY_BYTES,
            "scene.pkg entry {name:?} exceeds its 256 MiB budget"
        );
        entries.push(Entry { name, offset, size });
    }
    let data_start = cursor.position();
    ensure!(
        data_start <= bytes.len(),
        "scene.pkg entry table is truncated"
    );
    for entry in &mut entries {
        entry.offset = entry
            .offset
            .checked_add(data_start)
            .context("scene.pkg entry offset overflows")?;
    }
    crate::log_trace!(
        "scene.pkg {} ({version}) table {}..{} for {} entries",
        pkg_path.display(),
        table_start,
        data_start,
        entries.len()
    );
    Ok(entries)
}

fn validate_name(name: &str) -> Result<()> {
    ensure!(!name.is_empty(), "scene.pkg entry name is empty");
    let relative = Path::new(name);
    ensure!(
        !name.contains('\\'),
        "scene.pkg entry {name:?} uses a backslash separator"
    );
    ensure!(
        relative
            .components()
            .all(|item| matches!(item, Component::Normal(_) | Component::CurDir)),
        "scene.pkg entry {name:?} is not a relative path without parent traversal"
    );
    Ok(())
}

struct Cursor<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, position: 0 }
    }

    fn position(&self) -> usize {
        self.position
    }

    fn take(&mut self, length: usize) -> Result<&'a [u8]> {
        let end = self
            .position
            .checked_add(length)
            .context("scene.pkg cursor overflows")?;
        ensure!(end <= self.bytes.len(), "scene.pkg is truncated");
        let slice = &self.bytes[self.position..end];
        self.position = end;
        Ok(slice)
    }

    fn u32(&mut self) -> Result<u32> {
        let bytes = self.take(4)?;
        Ok(u32::from_le_bytes(bytes.try_into().unwrap()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut data = Vec::new();
        let mut table = Vec::new();
        table.extend_from_slice(&(8u32).to_le_bytes());
        table.extend_from_slice(b"PKGV0018");
        table.extend_from_slice(&(entries.len() as u32).to_le_bytes());
        for (name, payload) in entries {
            table.extend_from_slice(&(name.len() as u32).to_le_bytes());
            table.extend_from_slice(name.as_bytes());
            table.extend_from_slice(&(data.len() as u32).to_le_bytes());
            table.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            data.extend_from_slice(payload);
        }
        table.extend_from_slice(&data);
        table
    }

    fn parse_bytes(bytes: &[u8]) -> Result<Vec<Entry>> {
        let path = std::env::temp_dir().join(format!(
            "wallbreak-pkg-test-{}-{:x}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::write(&path, bytes).unwrap();
        let result = parse(&path);
        let _ = fs::remove_file(&path);
        result
    }

    #[test]
    fn parses_shipped_layout_and_validates_entries() {
        let bytes = build(&[("scene.json", b"{}"), ("materials/a.tex", b"TEXV0005")]);
        let entries = parse_bytes(&bytes).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "scene.json");
        assert_eq!(entries[1].name, "materials/a.tex");
        let scene = &bytes[entries[0].offset..entries[0].offset + entries[0].size];
        assert_eq!(scene, b"{}");
        let tex = &bytes[entries[1].offset..entries[1].offset + entries[1].size];
        assert_eq!(tex, b"TEXV0005");
    }

    #[test]
    fn rejects_traversal_absolute_duplicate_and_truncated_entries() {
        for name in ["../escape", "/etc/passwd", "a/../../b", "a\\b", ""] {
            let bytes = build(&[(name, b"x")]);
            assert!(parse_bytes(&bytes).is_err(), "{name:?} was accepted");
        }
        let bytes = build(&[("a", b"1"), ("a", b"2")]);
        assert!(parse_bytes(&bytes).is_err());
        let mut bytes = build(&[("scene.json", b"{}")]);
        bytes.truncate(20);
        assert!(parse_bytes(&bytes).is_err());
        let bytes = b"not a package";
        assert!(parse_bytes(bytes).is_err());
    }

    #[test]
    fn extracts_entries_into_staging_cache() {
        let bytes = build(&[
            ("scene.json", b"{\"objects\":[]}"),
            ("materials/a.tex", b"TEXV0005\x00"),
        ]);
        let root = std::env::temp_dir().join(format!(
            "wallbreak-pkg-extract-{}-{:x}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let pkg = root.join("scene.pkg");
        fs::create_dir_all(&root).unwrap();
        fs::write(&pkg, &bytes).unwrap();
        let cache = root.join("cache").join("key");
        let count = extract_to_cache(&pkg, &cache).unwrap();
        assert_eq!(count, 2);
        assert_eq!(
            fs::read(cache.join("scene.json")).unwrap(),
            b"{\"objects\":[]}"
        );
        assert_eq!(
            fs::read(cache.join("materials/a.tex")).unwrap(),
            b"TEXV0005\x00"
        );
        assert!(
            !cache
                .with_extension(format!("staging{}", std::process::id()))
                .exists()
        );
        let _ = fs::remove_dir_all(&root);
    }
}
