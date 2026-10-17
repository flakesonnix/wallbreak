use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    fs::{self, File, OpenOptions},
    io::{ErrorKind, Read, Write},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{bail, ensure, Context, Result};
use rustix::fs::OFlags;
use serde::{Deserialize, Serialize};

pub const SETTINGS_VERSION: u32 = 1;
const MAX_SETTINGS_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SortOrder {
    Title,
    Kind,
    Recent,
    Favorites,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PlaybackProfile {
    pub path: PathBuf,
    pub preview: bool,
    pub output: String,
    pub fps: u32,
    pub contain: bool,
    pub ffmpeg: PathBuf,
}

impl Default for PlaybackProfile {
    fn default() -> Self {
        Self {
            path: PathBuf::new(),
            preview: false,
            output: String::new(),
            fps: 30,
            contain: false,
            ffmpeg: PathBuf::from("ffmpeg"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LibrarySettings {
    pub version: u32,
    pub favorites: BTreeSet<PathBuf>,
    pub tags: BTreeMap<PathBuf, Vec<String>>,
    pub folders: Vec<PathBuf>,
    pub profiles: BTreeMap<String, PlaybackProfile>,
    pub last_used: BTreeMap<PathBuf, u64>,
    pub playback: PlaybackProfile,
    pub sort: SortOrder,
    pub favorites_only: bool,
    pub list_view: bool,
    pub card_size: f32,
    pub playlist: Vec<PathBuf>,
    pub interval_seconds: u64,
    pub shuffle: bool,
}

impl Default for LibrarySettings {
    fn default() -> Self {
        Self {
            version: SETTINGS_VERSION,
            favorites: BTreeSet::new(),
            tags: BTreeMap::new(),
            folders: Vec::new(),
            profiles: BTreeMap::new(),
            last_used: BTreeMap::new(),
            playback: PlaybackProfile::default(),
            sort: SortOrder::Title,
            favorites_only: false,
            list_view: false,
            card_size: 200.0,
            playlist: Vec::new(),
            interval_seconds: 300,
            shuffle: false,
        }
    }
}

impl LibrarySettings {
    pub fn mark_used(&mut self, path: &Path) {
        self.last_used.insert(
            path.to_path_buf(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
        );
    }

    fn validate(&self) -> Result<()> {
        ensure!(
            self.version <= SETTINGS_VERSION,
            "settings version {} is newer than supported version {SETTINGS_VERSION}",
            self.version
        );
        ensure!(
            self.version == SETTINGS_VERSION,
            "unsupported settings version {}",
            self.version
        );
        ensure!(
            (1..=60).contains(&self.playback.fps),
            "playback fps must be in 1..=60"
        );
        for (name, profile) in &self.profiles {
            ensure!(
                (1..=60).contains(&profile.fps),
                "profile {name:?} fps must be in 1..=60"
            );
        }
        ensure!(
            self.card_size.is_finite() && (120.0..=360.0).contains(&self.card_size),
            "card_size must be finite and in 120..=360"
        );
        ensure!(
            (1..=86400).contains(&self.interval_seconds),
            "interval_seconds must be in 1..=86400"
        );
        Ok(())
    }
}

pub fn settings_path() -> Result<PathBuf> {
    let base = env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| {
            env::var_os("HOME")
                .map(PathBuf::from)
                .filter(|path| path.is_absolute())
                .map(|path| path.join(".config"))
        })
        .context("settings require an absolute XDG_CONFIG_HOME or HOME")?;
    Ok(base.join("wallbreak/library.json"))
}

pub fn load(path: &Path) -> Result<LibrarySettings> {
    Ok(read_settings(path)?
        .map(|(_, settings)| settings)
        .unwrap_or_default())
}

pub fn save(path: &Path, settings: &LibrarySettings) -> Result<()> {
    settings.validate()?;
    let mut bytes = serde_json::to_vec_pretty(settings).context("cannot serialize settings")?;
    bytes.push(b'\n');
    ensure!(
        bytes.len() as u64 <= MAX_SETTINGS_BYTES,
        "settings exceed 4 MiB"
    );
    let existing = read_settings(path)?;
    if existing.as_ref().is_some_and(|(old, _)| *old == bytes) {
        return Ok(());
    }

    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    ensure!(path.file_name().is_some(), "settings path must name a file");
    fs::create_dir_all(parent)
        .with_context(|| format!("cannot create settings directory {}", parent.display()))?;

    if let Some((previous, _)) = existing {
        let mut backup = path.as_os_str().to_os_string();
        backup.push(".bak");
        write_atomic(Path::new(&backup), &previous)?;
    }
    write_atomic(path, &bytes)
}

fn read_settings(path: &Path) -> Result<Option<(Vec<u8>, LibrarySettings)>> {
    // Nonblocking open lets us reject FIFOs and devices without waiting for a writer.
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(OFlags::NONBLOCK.bits() as i32)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("cannot open settings {}", path.display()))
        }
    };
    let metadata = file
        .metadata()
        .with_context(|| format!("cannot inspect settings {}", path.display()))?;
    ensure!(metadata.is_file(), "settings must be a regular file");
    ensure!(
        metadata.len() <= MAX_SETTINGS_BYTES,
        "settings exceed 4 MiB"
    );
    let mut bytes = Vec::new();
    file.take(MAX_SETTINGS_BYTES + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("cannot read settings {}", path.display()))?;
    ensure!(
        bytes.len() as u64 <= MAX_SETTINGS_BYTES,
        "settings exceed 4 MiB"
    );
    let value: serde_json::Value = serde_json::from_slice(&bytes)
        .with_context(|| format!("malformed settings in {}", path.display()))?;
    ensure!(value.is_object(), "settings must be a JSON object");
    let settings: LibrarySettings = serde_json::from_value(value)
        .with_context(|| format!("invalid settings in {}", path.display()))?;
    settings
        .validate()
        .with_context(|| format!("invalid settings in {}", path.display()))?;
    Ok(Some((bytes, settings)))
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let name = path.file_name().context("settings path must name a file")?;
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    for attempt in 0..64 {
        let mut temporary_name = name.to_os_string();
        temporary_name.push(format!(".{}.{}.{attempt}.tmp", std::process::id(), stamp));
        let temporary = parent.join(temporary_name);
        let mut file = match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)
        {
            Ok(file) => file,
            Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("cannot create temporary settings {}", temporary.display())
                })
            }
        };
        let result = (|| -> Result<()> {
            file.write_all(bytes)
                .with_context(|| format!("cannot write settings {}", temporary.display()))?;
            file.sync_all()
                .with_context(|| format!("cannot sync settings {}", temporary.display()))?;
            fs::rename(&temporary, path)
                .with_context(|| format!("cannot replace settings {}", path.display()))?;
            File::open(parent)
                .and_then(|directory| directory.sync_all())
                .with_context(|| {
                    format!(
                        "settings {} were replaced but the directory could not be synced",
                        path.display()
                    )
                })?;
            Ok(())
        })();
        drop(file);
        if result.is_err() {
            if let Err(error) = fs::remove_file(&temporary) {
                if error.kind() != ErrorKind::NotFound {
                    return result.with_context(|| {
                        format!(
                            "also cannot remove temporary settings {}: {error}",
                            temporary.display()
                        )
                    });
                }
            }
        }
        return result;
    }
    bail!(
        "cannot allocate a unique temporary file for {}",
        path.display()
    )
}
