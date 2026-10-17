//! Independent renderer for one narrow Wallpaper Engine scene profile.
//!
//! The accepted profile is currently exactly one zero-rotation image quad with
//! one `flag`-shader pass, three native textures, and user tint properties.
//! Anything else fails before decoding. In particular, a static preview is never
//! substituted for scene rendering.
//!
//! The visual model is derived from the installed Eagle Flag project's public
//! declarations and its shared `flag.vert`/`flag.frag` sources. It is a new
//! CPU implementation, not recovered engine code. Camera eye/center metadata
//! are retained but do not move the accepted orthographic quad: the shipped
//! camera is offset while the object and preview remain canvas-centered.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    io::Read,
    path::{Path, PathBuf},
};

use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};

use crate::{
    media::{frame_len, Fit},
    project::open_regular_file,
    texture::{load_any as load_texture, Texture},
};

const MAX_SCENE_BYTES: u64 = 4 * 1024 * 1024;
const MAX_JSON_BYTES: u64 = 1024 * 1024;
// Shipped 2048px scenes with three full layers need ~65 MiB decoded.
const MAX_TEXTURE_RGBA_TOTAL: usize = 128 * 1024 * 1024;

#[derive(Debug, Clone, Serialize)]
pub struct SceneProfile {
    pub scene_path: PathBuf,
    pub projection: [u32; 2],
    pub clear_color: [f32; 3],
    pub camera: Camera,
    pub layers: Vec<LayerProfile>,
    pub limitations: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct LayerProfile {
    pub id: u32,
    pub name: String,
    pub center: [f32; 2],
    pub extents: [f32; 2],
    pub rotation: f32,
    pub model: [u32; 2],
    pub material: MaterialProfile,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MaterialProfile {
    Flag {
        colors: FlagColors,
        speed: f32,
        strength: f32,
        textures: [String; 3],
    },
    Generic {
        brightness: f32,
        alpha: f32,
        power: f32,
        scroll: [f32; 2],
        blending: Blending,
        texture: String,
    },
    Flow {
        brightness: f32,
        speed: f32,
        amount: f32,
        bias: [f32; 2],
        base: String,
        mask: String,
    },
    FlowLayers {
        brightness: f32,
        amount: f32,
        speeds: [f32; 3],
        bias: [f32; 2],
        mask: String,
        layers: [String; 3],
    },
    Sway {
        brightness: f32,
        speed: f32,
        amount: f32,
        base: String,
        mask: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Blending {
    Opaque,
    Translucent,
    Additive,
}

#[derive(Debug, Clone, Serialize)]
pub struct Camera {
    pub eye: [f32; 3],
    pub center: [f32; 3],
    pub up: [f32; 3],
}

#[derive(Debug, Clone)]
pub struct LayerGeometry {
    pub center: [f32; 2],
    pub extents: [f32; 2],
    pub rotation: f32,
}

#[derive(Debug, Clone)]
pub struct Scene {
    profile: SceneProfile,
    layers: Vec<LayerRuntime>,
}

#[derive(Debug, Clone)]
pub(crate) struct LayerRuntime {
    pub(crate) geometry: LayerGeometry,
    pub(crate) material: MaterialRuntime,
}

impl MaterialProfile {
    pub(crate) fn texture_count(&self) -> usize {
        match self {
            MaterialProfile::Flag { .. } => 3,
            MaterialProfile::Generic { .. } => 1,
            MaterialProfile::Flow { .. } => 2,
            MaterialProfile::FlowLayers { .. } => 4,
            MaterialProfile::Sway { .. } => 2,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) enum MaterialRuntime {
    Flag {
        colors: FlagColors,
        speed: f32,
        strength: f32,
        base: Texture,
        normal: Texture,
        cloth: Texture,
    },
    Generic {
        brightness: f32,
        alpha: f32,
        power: f32,
        scroll: [f32; 2],
        blending: Blending,
        tex: Texture,
    },
    Flow {
        brightness: f32,
        speed: f32,
        amount: f32,
        bias: [f32; 2],
        base: Texture,
        mask: Texture,
    },
    FlowLayers {
        brightness: f32,
        amount: f32,
        speeds: [f32; 3],
        bias: [f32; 2],
        mask: Texture,
        layers: [Texture; 3],
    },
    Sway {
        brightness: f32,
        speed: f32,
        amount: f32,
        base: Texture,
        mask: Texture,
    },
}

impl MaterialRuntime {
    fn bind(profile: &MaterialProfile, mut textures: Vec<Texture>) -> Result<Self> {
        ensure!(
            textures.len() == profile.texture_count(),
            "scene texture budget miscounted its layers"
        );
        let mut take = || textures.remove(0);
        Ok(match profile {
            MaterialProfile::Flag {
                colors,
                speed,
                strength,
                ..
            } => MaterialRuntime::Flag {
                colors: colors.clone(),
                speed: *speed,
                strength: *strength,
                base: take(),
                normal: take(),
                cloth: take(),
            },
            MaterialProfile::Generic {
                brightness,
                alpha,
                power,
                scroll,
                blending,
                ..
            } => MaterialRuntime::Generic {
                brightness: *brightness,
                alpha: *alpha,
                power: *power,
                scroll: *scroll,
                blending: *blending,
                tex: take(),
            },
            MaterialProfile::Flow {
                brightness,
                speed,
                amount,
                bias,
                ..
            } => MaterialRuntime::Flow {
                brightness: *brightness,
                speed: *speed,
                amount: *amount,
                bias: *bias,
                base: take(),
                mask: take(),
            },
            MaterialProfile::FlowLayers {
                brightness,
                amount,
                speeds,
                bias,
                ..
            } => MaterialRuntime::FlowLayers {
                brightness: *brightness,
                amount: *amount,
                speeds: *speeds,
                bias: *bias,
                mask: take(),
                layers: [take(), take(), take()],
            },
            MaterialProfile::Sway {
                brightness,
                speed,
                amount,
                ..
            } => MaterialRuntime::Sway {
                brightness: *brightness,
                speed: *speed,
                amount: *amount,
                base: take(),
                mask: take(),
            },
        })
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct FlagColors {
    pub background: [f32; 3],
    pub accent1: [f32; 3],
    pub accent2: [f32; 3],
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct SceneLayout {
    pub(crate) factor: f64,
    pub(crate) left: f64,
    pub(crate) top: f64,
}

/// Layer compositing: Opaque replaces, Translucent uses straight alpha
/// (SRC_ALPHA, ONE_MINUS_SRC_ALPHA), Additive uses (SRC_ALPHA, ONE).
fn composite(dst: [f32; 4], src: [f32; 4], blending: Blending) -> [f32; 4] {
    match blending {
        Blending::Opaque => [src[0], src[1], src[2], dst[3]],
        Blending::Translucent => [
            src[0] * src[3] + dst[0] * (1.0 - src[3]),
            src[1] * src[3] + dst[1] * (1.0 - src[3]),
            src[2] * src[3] + dst[2] * (1.0 - src[3]),
            src[3] + dst[3] * (1.0 - src[3]),
        ],
        Blending::Additive => [
            src[0] * src[3] + dst[0],
            src[1] * src[3] + dst[1],
            src[2] * src[3] + dst[2],
            dst[3].max(src[3]),
        ],
    }
}

#[derive(Debug, Clone)]
pub struct SceneFrame {
    scene: Scene,
    width: u32,
    height: u32,
    fit: Fit,
    started: std::time::Instant,
}

#[derive(Debug, Deserialize)]
struct SceneDocument {
    camera: CameraDocument,
    general: GeneralDocument,
    objects: Vec<ObjectDocument>,
    #[serde(default, flatten)]
    extra: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct CameraDocument {
    center: String,
    eye: String,
    up: String,
    #[serde(default)]
    paths: Vec<String>,
    #[serde(default, flatten)]
    extra: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct GeneralDocument {
    ambientcolor: Option<String>,
    #[serde(default)]
    bloom: Option<BloomSetting>,
    clearcolor: String,
    norecompile: Option<bool>,
    #[serde(default)]
    orthogonalprojection: Option<ProjectionDocument>,
    skylightcolor: Option<String>,
    #[serde(default, flatten)]
    extra: HashMap<String, serde_json::Value>,
}

/// `bloom` is a boolean in most versions; newer scenes configure it with an
/// object. Both forms mean bloom is enabled.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum BloomSetting {
    Plain(bool),
    Configured(serde_json::Value),
}

impl BloomSetting {
    fn enabled(&self) -> bool {
        match self {
            Self::Plain(enabled) => *enabled,
            Self::Configured(_) => true,
        }
    }
}

#[derive(Debug, Deserialize)]
struct ProjectionDocument {
    height: u32,
    width: u32,
}

/// A scene value that may be a plain string or a `{value, script}` object.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ScriptedValue {
    Plain(String),
    Scripted {
        #[serde(default)]
        value: Option<String>,
        #[serde(default)]
        script: Option<String>,
    },
}

impl ScriptedValue {
    fn value(&self) -> Option<&str> {
        match self {
            Self::Plain(value) => Some(value),
            Self::Scripted { value, .. } => value.as_deref(),
        }
    }

    fn is_scripted(&self) -> bool {
        matches!(self, Self::Scripted { .. })
    }
}

/// Object visibility may be a boolean or a condition object.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum VisibleValue {
    Plain(bool),
    Conditional(serde_json::Value),
}

impl VisibleValue {
    fn hidden(&self) -> bool {
        matches!(self, Self::Plain(false))
    }

    fn is_conditional(&self) -> bool {
        matches!(self, Self::Conditional(_))
    }
}

#[derive(Debug, Deserialize)]
struct ObjectDocument {
    #[serde(default)]
    id: u32,
    #[serde(default)]
    name: String,
    #[serde(default)]
    angles: Option<ScriptedValue>,
    #[serde(default)]
    origin: Option<ScriptedValue>,
    #[serde(default)]
    scale: Option<ScriptedValue>,
    #[serde(default)]
    image: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    visible: Option<VisibleValue>,
    #[serde(default, flatten)]
    extra: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct CameraPathDocument {
    paths: Vec<CameraPathTrack>,
}

#[derive(Debug, Deserialize)]
struct CameraPathTrack {
    duration: f32,
    #[serde(default)]
    name: Option<String>,
    transforms: Vec<CameraTransformDocument>,
}

#[derive(Debug, Deserialize)]
struct CameraTransformDocument {
    center: String,
    eye: String,
    up: String,
    timestamp: f32,
}

#[derive(Deserialize)]
struct ModelDocument {
    #[serde(default)]
    height: Option<u32>,
    material: String,
    #[serde(default)]
    width: Option<u32>,
    #[serde(default)]
    autosize: bool,
    #[serde(default, flatten)]
    extra: HashMap<String, serde_json::Value>,
}

#[derive(Deserialize)]
struct MaterialDocument {
    passes: Vec<PassDocument>,
    #[serde(default, flatten)]
    extra: HashMap<String, serde_json::Value>,
}

#[derive(Deserialize)]
struct PassDocument {
    #[serde(default)]
    combos: HashMap<String, u8>,
    #[serde(default)]
    constantshadervalues: HashMap<String, f32>,
    depthtest: String,
    depthwrite: String,
    shader: String,
    textures: Vec<Option<String>>,
    #[serde(default)]
    usershadervalues: HashMap<String, String>,
    blending: Option<String>,
    cullmode: Option<String>,
    #[serde(default, flatten)]
    extra: HashMap<String, serde_json::Value>,
}

/// Validated scene metadata without decoded textures.
#[derive(Debug, Clone)]
pub struct SceneSpec {
    pub profile: SceneProfile,
    pub texture_paths: Vec<PathBuf>,
}

/// Load and validate one layered scene project selection.
///
/// `properties` are the project's declared color values, required only by
/// flag materials. The scene file itself must already be constrained to the
/// project directory by the caller.
pub fn load(
    scene_path: &Path,
    project_root: &Path,
    properties: &HashMap<String, [f32; 3]>,
) -> Result<Scene> {
    let mut spec = parse(scene_path, project_root, properties)?;
    let mut textures: Vec<Option<Texture>> = Vec::with_capacity(spec.texture_paths.len());
    let mut rgba_total = 0usize;
    for path in &spec.texture_paths {
        match load_texture(path) {
            Ok(texture) => {
                rgba_total = rgba_total
                    .checked_add(texture.rgba.len())
                    .context("scene texture total overflow")?;
                ensure!(
                    rgba_total <= MAX_TEXTURE_RGBA_TOTAL,
                    "scene textures exceed 64 MiB decoded budget"
                );
                textures.push(Some(texture));
            }
            Err(error) => {
                crate::log_debug!(
                    "scene texture {} cannot be decoded: {error:#}",
                    path.display()
                );
                textures.push(None);
            }
        }
    }
    let mut layers = Vec::with_capacity(spec.profile.layers.len());
    let mut kept = Vec::with_capacity(spec.profile.layers.len());
    let mut texture_index = 0usize;
    let mut dropped: Vec<String> = Vec::new();
    for layer in &spec.profile.layers {
        let count = layer.material.texture_count();
        let slice = textures
            .get(texture_index..texture_index + count)
            .context("scene texture budget miscounted its layers")?;
        texture_index += count;
        if slice.iter().all(Option::is_some) {
            let owned = slice.iter().flatten().cloned().collect();
            layers.push(LayerRuntime {
                geometry: LayerGeometry {
                    center: layer.center,
                    extents: layer.extents,
                    rotation: layer.rotation,
                },
                material: MaterialRuntime::bind(&layer.material, owned)?,
            });
            kept.push(layer.clone());
        } else {
            dropped.push(format!(
                "object {:?} is not rendered: one of its textures cannot be decoded",
                layer.name
            ));
        }
    }
    // Loading fails only when parsing and decoding together leave nothing
    // renderable; an explicitly invisible scene still renders its clear color.
    ensure!(
        !layers.is_empty() || spec.profile.layers.is_empty(),
        "scene has no decodable image objects ({})",
        join_bounded(&dropped, 6)
    );
    if !dropped.is_empty() {
        spec.profile.limitations.extend(dropped);
    }
    spec.profile.layers = kept;
    Ok(Scene {
        profile: spec.profile,
        layers,
    })
}

/// Validate one layered scene project selection without decoding textures.
///
/// Used to mark Steam projects as supported before the user selects one.
/// Unknown fields and unsupported object kinds are tolerated and summarized in
/// `profile.limitations`; a scene fails only when nothing renderable remains.
pub fn parse(
    scene_path: &Path,
    project_root: &Path,
    properties: &HashMap<String, [f32; 3]>,
) -> Result<SceneSpec> {
    let document: SceneDocument =
        read_json(scene_path, MAX_SCENE_BYTES).context("invalid scene.json")?;
    let camera = Camera {
        eye: parse_triplet(&document.camera.eye).context("invalid camera eye")?,
        center: parse_triplet(&document.camera.center).context("invalid camera center")?,
        up: parse_triplet(&document.camera.up).context("invalid camera up")?,
    };
    let mut derived_note: Option<String> = None;
    let projection = match &document.general.orthogonalprojection {
        Some(projection) if projection.width > 0 && projection.height > 0 => {
            [projection.width, projection.height]
        }
        other => {
            // Perspective or malformed projection: approximate the visible
            // frustum slice at the camera focus distance as an orthographic view.
            let depth = [
                camera.center[0] - camera.eye[0],
                camera.center[1] - camera.eye[1],
                camera.center[2] - camera.eye[2],
            ];
            let distance = f64::from(
                (depth[0] * depth[0] + depth[1] * depth[1] + depth[2] * depth[2])
                    .sqrt()
                    .clamp(0.1, 10_000.0),
            );
            let fov = document
                .general
                .extra
                .get("fov")
                .and_then(serde_json::Value::as_f64)
                .filter(|fov| fov.is_finite() && *fov > 1.0 && *fov < 179.0)
                .unwrap_or(50.0);
            let height = (2.0 * (fov.to_radians() / 2.0).tan() * distance).clamp(0.1, 32_768.0);
            let width = (height * 16.0 / 9.0).clamp(0.1, 32_768.0);
            let derived = [width.round() as u32, height.round() as u32];
            let note = if other.is_some() {
                format!(
                    "Orthogonal projection is zero-sized; approximated from camera distance at {fov:.0}° fov: {}x{} units.",
                    derived[0], derived[1]
                )
            } else {
                format!(
                    "Scene has no orthogonal projection; the perspective view is approximated orthographically from camera distance at {fov:.0}° fov: {}x{} units.",
                    derived[0], derived[1]
                )
            };
            derived_note = Some(note);
            derived
        }
    };
    ensure!(
        u64::from(projection[0]) * u64::from(projection[1]) <= 67_108_864,
        "scene projection exceeds 64 megapixels"
    );
    let clear_color = parse_triplet(&document.general.clearcolor).context("invalid clearcolor")?;

    if let Some(ambient) = &document.general.ambientcolor {
        parse_triplet(ambient).context("invalid ambientcolor")?;
    }
    if let Some(sky) = &document.general.skylightcolor {
        parse_triplet(sky).context("invalid skylightcolor")?;
    }
    let _ = document.general.norecompile;

    let mut limitations = vec![
        "Renders orthographic image quads back-to-front with flag, generic, flow, and sway materials; camera eye/center metadata are retained, not used for placement.".into(),
        "Uses native TEX or raw image textures; animation plays its first frame only.".into(),
        "CPU reimplementation of the shipped vertex/fragment behavior; no engine code is executed.".into(),
    ];
    if let Some(note) = derived_note.take() {
        limitations.push(note);
    }
    if let Some(setting) = document.general.bloom.as_ref() {
        if setting.enabled() {
            if let BloomSetting::Configured(config) = setting {
                crate::log_debug!("scene bloom is configured but not applied: {config}");
            }
            limitations.push("Bloom post-processing is not applied.".into());
        }
    }

    let mut layers = Vec::with_capacity(document.objects.len());
    let mut texture_paths = Vec::new();
    let mut skipped: BTreeMap<&'static str, usize> = BTreeMap::new();
    let mut failures: Vec<String> = Vec::new();
    let mut ignored_object_keys: BTreeSet<String> = BTreeSet::new();
    let mut hidden = 0usize;
    let mut conditional_visibility = 0usize;
    let mut scripted_transforms = 0usize;
    let mut notes: BTreeSet<String> = BTreeSet::new();
    for object in &document.objects {
        if object.visible.as_ref().is_some_and(VisibleValue::hidden) {
            hidden += 1;
            continue;
        }
        if object
            .visible
            .as_ref()
            .is_some_and(VisibleValue::is_conditional)
        {
            if let Some(VisibleValue::Conditional(condition)) = object.visible.as_ref() {
                crate::log_trace!(
                    "object {:?} visibility condition is not evaluated: {condition}",
                    object_label(object)
                );
            }
            conditional_visibility += 1;
        }
        if [&object.angles, &object.origin, &object.scale]
            .iter()
            .any(|value| value.as_ref().is_some_and(ScriptedValue::is_scripted))
        {
            scripted_transforms += 1;
        }
        let Some(image) = &object.image else {
            *skipped.entry(skipped_object_category(object)).or_default() += 1;
            continue;
        };
        match parse_layer(object, image, project_root, properties, &mut notes) {
            Ok((layer, paths)) => {
                for key in object.extra.keys() {
                    ignored_object_keys.insert(key.clone());
                }
                texture_paths.extend(paths);
                layers.push(layer);
            }
            Err(error) => failures.push(format!(
                "object {:?} is not rendered: {error:#}",
                object_label(object)
            )),
        }
    }
    let rendered = layers.len();
    let visible_total = document.objects.len().saturating_sub(hidden);
    // An explicitly invisible scene still renders (its clear color); anything
    // else with no renderable object fails with the recorded reasons.
    ensure!(
        !layers.is_empty() || hidden > 0,
        "scene has no renderable image objects ({})",
        omitted_summary(&skipped, &failures, hidden)
    );
    if rendered < visible_total {
        let mut omitted: Vec<String> = skipped
            .iter()
            .map(|(category, count)| format!("{count} {category}"))
            .collect();
        omitted.extend(failures.iter().cloned());
        limitations.push(format!(
            "Renders {rendered} of {visible_total} objects; the rest are omitted: {}.",
            join_bounded(&omitted, 6)
        ));
    }
    if hidden > 0 {
        limitations.push(format!("{hidden} object(s) are declared invisible."));
    }
    if scripted_transforms > 0 {
        limitations.push(format!(
            "{scripted_transforms} object transform(s) are scripted; their declared static values are used."
        ));
    }
    if conditional_visibility > 0 {
        limitations.push(format!(
            "{conditional_visibility} object visibility condition(s) are not evaluated; those objects are shown."
        ));
    }
    limitations.extend(notes);
    if !document.camera.paths.is_empty() {
        limitations.push(
            match load_camera_paths(project_root, &document.camera.paths) {
                Ok((tracks, total)) => format!(
                    "Camera path animation ({tracks} track(s), {total:.1}s per cycle) is not applied; the declared camera transform stays static."
                ),
                Err(error) => {
                    format!("Camera path scripts were not applied: {error:#}.")
                }
            },
        );
    }
    for (label, keys) in [
        ("Ignored scene options", keys_of(&document.general.extra)),
        ("Ignored camera options", keys_of(&document.camera.extra)),
        ("Ignored scene fields", keys_of(&document.extra)),
        (
            "Ignored object properties",
            ignored_object_keys.into_iter().collect(),
        ),
    ] {
        if !keys.is_empty() {
            limitations.push(format!("{label}: {}.", join_bounded(&keys, 8)));
        }
    }
    let profile = SceneProfile {
        scene_path: scene_path.to_path_buf(),
        projection,
        clear_color,
        camera,
        layers,
        limitations,
    };
    Ok(SceneSpec {
        profile,
        texture_paths,
    })
}

fn keys_of(map: &HashMap<String, serde_json::Value>) -> Vec<String> {
    map.keys().cloned().collect()
}

fn skipped_object_category(object: &ObjectDocument) -> &'static str {
    if object.model.is_some() {
        "3D models"
    } else if object.extra.contains_key("particle") || object.extra.contains_key("particlesrc") {
        "particle systems"
    } else if object.extra.contains_key("text") {
        "text objects"
    } else if object.extra.contains_key("sound") {
        "sound objects"
    } else if object.extra.contains_key("light") {
        "light objects"
    } else if object.extra.contains_key("sprite") {
        "sprite objects"
    } else if object.extra.contains_key("solid") {
        "solid shapes"
    } else {
        "objects without image or model"
    }
}

fn omitted_summary(
    skipped: &BTreeMap<&'static str, usize>,
    failures: &[String],
    hidden: usize,
) -> String {
    let mut parts: Vec<String> = skipped
        .iter()
        .map(|(category, count)| format!("{count} {category}"))
        .collect();
    parts.extend(failures.iter().cloned());
    if hidden > 0 {
        parts.push(format!("{hidden} invisible"));
    }
    if parts.is_empty() {
        parts.push("no image objects declared".into());
    }
    join_bounded(&parts, 6)
}

fn join_bounded(parts: &[String], limit: usize) -> String {
    if parts.len() <= limit {
        return parts.join(", ");
    }
    format!(
        "{}, and {} more",
        parts[..limit].join(", "),
        parts.len() - limit
    )
}

/// Validate the declared camera path scripts and report track count and cycle
/// duration. The scripts are not applied to rendering yet.
fn load_camera_paths(project_root: &Path, paths: &[String]) -> Result<(usize, f32)> {
    ensure!(
        paths.len() <= 16,
        "camera declares {} path scripts",
        paths.len()
    );
    let mut tracks = 0usize;
    let mut total = 0.0f32;
    for declaration in paths {
        let path = root_relative(project_root, declaration)?;
        let document: CameraPathDocument = read_json(&path, MAX_JSON_BYTES)?;
        ensure!(
            !document.paths.is_empty(),
            "camera path script {declaration} has no tracks"
        );
        for track in &document.paths {
            ensure!(
                track.duration.is_finite() && track.duration > 0.0,
                "camera path track has invalid duration"
            );
            ensure!(
                !track.transforms.is_empty(),
                "camera path track has no transforms"
            );
            let _ = &track.name;
            for transform in &track.transforms {
                ensure!(transform.timestamp.is_finite(), "invalid camera timestamp");
                parse_triplet(&transform.center).context("invalid camera path center")?;
                parse_triplet(&transform.eye).context("invalid camera path eye")?;
                parse_triplet(&transform.up).context("invalid camera path up")?;
            }
            tracks += 1;
            total += track.duration;
        }
    }
    Ok((tracks, total))
}

fn object_label(object: &ObjectDocument) -> String {
    if !object.name.is_empty() {
        return object.name.clone();
    }
    if let Some(image) = &object.image {
        return image.clone();
    }
    if let Some(model) = &object.model {
        return model.clone();
    }
    format!("object {}", object.id)
}

fn parse_layer(
    object: &ObjectDocument,
    image: &str,
    project_root: &Path,
    properties: &HashMap<String, [f32; 3]>,
    notes: &mut BTreeSet<String>,
) -> Result<(LayerProfile, Vec<PathBuf>)> {
    let component = |value: &Option<ScriptedValue>| {
        value
            .as_ref()
            .and_then(ScriptedValue::value)
            .map(str::to_string)
    };
    for (field, value) in [
        ("angles", &object.angles),
        ("origin", &object.origin),
        ("scale", &object.scale),
    ] {
        if let Some(ScriptedValue::Scripted { script, .. }) = value {
            crate::log_debug!(
                "object {:?} {field} is scripted{}; using its declared static value",
                object_label(object),
                script
                    .as_deref()
                    .map(|script| format!(" by {script:?}"))
                    .unwrap_or_default()
            );
        }
    }
    let angles = parse_triplet(component(&object.angles).as_deref().unwrap_or("0 0 0"))
        .context("invalid object angles")?;
    ensure!(
        angles[0] == 0.0 && angles[1] == 0.0 && angles[2].is_finite(),
        "unsupported object rotation {angles:?}; only in-plane rotation is supported"
    );
    let rotation = angles[2];
    let origin = parse_triplet(component(&object.origin).as_deref().unwrap_or("0 0 0"))
        .context("invalid object origin")?;
    let scale = parse_triplet(component(&object.scale).as_deref().unwrap_or("1 1 1"))
        .context("invalid object scale")?;
    ensure!(
        scale[0].is_finite() && scale[1].is_finite() && scale[0] > 0.0 && scale[1] > 0.0,
        "unsupported object scale {scale:?}"
    );
    let model_path = root_relative(project_root, image).context("invalid scene model")?;
    let model: ModelDocument =
        read_json(&model_path, MAX_JSON_BYTES).context("invalid scene model")?;
    for key in model.extra.keys() {
        crate::log_trace!("ignored model key {key:?} in {}", model_path.display());
    }
    let material_path =
        root_relative(project_root, &model.material).context("invalid scene material")?;
    let material: MaterialDocument =
        read_json(&material_path, MAX_JSON_BYTES).context("invalid scene material")?;
    for key in material.extra.keys() {
        crate::log_trace!(
            "ignored material key {key:?} in {}",
            material_path.display()
        );
    }
    ensure!(
        material.passes.len() == 1,
        "unsupported object with {} material passes",
        material.passes.len()
    );
    let material_root = material_path.parent().context("material has no parent")?;
    let project_root = project_root
        .canonicalize()
        .context("cannot resolve scene project root")?;
    let (profile, stems) = parse_material(&material.passes[0], &project_root, properties, notes)?;
    let mut paths = Vec::with_capacity(stems.len());
    for stem in &stems {
        paths.push(resolve_texture(material_root, &project_root, stem)?);
    }
    let (width, height) = match (model.width, model.height) {
        (Some(width), Some(height)) if width > 0 && height > 0 && !model.autosize => {
            (width, height)
        }
        _ => {
            // Autosized (or underspecified) models take the base texture size.
            let (width, height) = crate::texture::dimensions(&paths[0])
                .with_context(|| format!("cannot autosize model {:?}", object_label(object)))?;
            crate::log_debug!(
                "scene model {:?} is autosized from {}: {width}x{height}",
                object_label(object),
                paths[0].display()
            );
            (width, height)
        }
    };
    Ok((
        LayerProfile {
            id: object.id,
            name: object.name.clone(),
            center: [origin[0], origin[1]],
            extents: [(width as f32) * scale[0], (height as f32) * scale[1]],
            rotation,
            model: [width, height],
            material: profile,
        },
        paths,
    ))
}

/// Resolve one material texture name against the material's directory. Native
/// TEX wins, then raw image files the `image` crate can decode.
fn resolve_texture(material_root: &Path, project_root: &Path, stem: &str) -> Result<PathBuf> {
    let relative = Path::new(stem);
    let mut candidates = Vec::new();
    if relative.extension().is_some() {
        candidates.push(relative.to_path_buf());
    } else {
        for extension in ["tex", "png", "jpg", "jpeg", "gif", "webp"] {
            candidates.push(Path::new(&format!("{stem}.{extension}")).to_path_buf());
        }
    }
    let mut last_error = None;
    for candidate in &candidates {
        let declaration = candidate.to_string_lossy();
        match scene_relative(material_root, project_root, &declaration) {
            Ok(path) => return Ok(path),
            Err(error) => last_error = Some(error),
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("no texture candidates for {stem:?}")))
}

fn finite(constants: &HashMap<String, f32>, name: &str) -> Result<f32> {
    let value = *constants
        .get(name)
        .with_context(|| format!("material constant {name} is missing"))?;
    ensure!(value.is_finite(), "material constant {name} must be finite");
    Ok(value)
}

fn optional_finite(constants: &HashMap<String, f32>, name: &str, default: f32) -> Result<f32> {
    match constants.get(name) {
        Some(value) => {
            ensure!(value.is_finite(), "material constant {name} must be finite");
            Ok(*value)
        }
        None => Ok(default),
    }
}

fn parse_material(
    pass: &PassDocument,
    project_root: &Path,
    properties: &HashMap<String, [f32; 3]>,
    notes: &mut BTreeSet<String>,
) -> Result<(MaterialProfile, Vec<String>)> {
    for key in pass.extra.keys() {
        crate::log_trace!(
            "ignored material pass key {key:?} in shader {:?}",
            pass.shader
        );
    }
    if let Some(usertextures) = pass.extra.get("usertextures") {
        if usertextures
            .as_array()
            .is_some_and(|entries| !entries.is_empty())
        {
            notes.insert(
                "User-selected textures are not applied; bundled textures are rendered.".into(),
            );
        }
    }
    ensure!(
        pass.depthtest == "disabled" && pass.depthwrite == "disabled",
        "unsupported depth mode {:?}/{:?}; only disabled depth is supported",
        pass.depthtest,
        pass.depthwrite
    );
    if let Some(cullmode) = &pass.cullmode {
        ensure!(cullmode == "nocull", "unsupported cull mode {cullmode:?}");
    }
    let blending = match pass.blending.as_deref() {
        None | Some("opaque") => Blending::Opaque,
        Some("translucent") | Some("normal") => Blending::Translucent,
        Some("additive") => Blending::Additive,
        Some(other) => anyhow::bail!("unsupported blending {other:?}"),
    };
    let textures: Vec<String> = pass.textures.iter().flatten().cloned().collect();
    match pass.shader.as_str() {
        "flag" => {
            ensure!(
                pass.combos.get("TINT") == Some(&1),
                "unsupported flag combos; only TINT=1 is supported"
            );
            ensure!(
                pass.textures.len() == 3,
                "unsupported flag texture count {}",
                pass.textures.len()
            );
            ensure!(
                textures.len() == 3,
                "unsupported flag texture slots; all three must be declared"
            );
            ensure!(
                pass.usershadervalues.get("schemecolor").map(String::as_str) == Some("color1")
                    && pass.usershadervalues.get("flagcolor1").map(String::as_str)
                        == Some("color2")
                    && pass.usershadervalues.get("flagcolor2").map(String::as_str)
                        == Some("color3"),
                "unsupported flag color bindings"
            );
            Ok((
                MaterialProfile::Flag {
                    colors: FlagColors {
                        background: *properties
                            .get("schemecolor")
                            .context("scene project does not declare schemecolor")?,
                        accent1: *properties
                            .get("flagcolor1")
                            .context("scene project does not declare flagcolor1")?,
                        accent2: *properties
                            .get("flagcolor2")
                            .context("scene project does not declare flagcolor2")?,
                    },
                    speed: finite(&pass.constantshadervalues, "Speed")?,
                    strength: finite(&pass.constantshadervalues, "Strength")?,
                    textures: [
                        textures[0].clone(),
                        textures[1].clone(),
                        textures[2].clone(),
                    ],
                },
                textures,
            ))
        }
        "genericimage" | "genericimage2" | "genericimage4" => {
            for (name, value) in &pass.combos {
                match name.to_ascii_uppercase().as_str() {
                    "VERSION" => {
                        notes.insert(format!(
                            "Material variant VERSION={value} is rendered as the base image."
                        ));
                    }
                    "LIGHTING" | "REFLECTION" => ensure!(
                        *value == 0,
                        "material {name}={value} enables per-pixel {name}; not supported"
                    ),
                    _ => anyhow::bail!("unsupported generic combo {name:?}"),
                }
            }
            ensure!(
                pass.usershadervalues.is_empty(),
                "unsupported generic user shader values"
            );
            ensure!(
                !textures.is_empty(),
                "unsupported generic material without a texture"
            );
            if textures.len() > 1 {
                notes.insert(format!(
                    "{} additional material map(s) are ignored.",
                    textures.len() - 1
                ));
            }
            for name in pass.constantshadervalues.keys() {
                ensure!(
                    matches!(
                        name.as_str(),
                        "Bright" | "Alpha" | "Power" | "Scroll 1 X" | "Scroll 1 Y"
                    ),
                    "unsupported generic constant {name:?}"
                );
            }
            let power = optional_finite(&pass.constantshadervalues, "Power", 1.0)?;
            ensure!(power >= 0.0, "generic Power must be nonnegative");
            Ok((
                MaterialProfile::Generic {
                    brightness: optional_finite(&pass.constantshadervalues, "Bright", 1.0)?,
                    alpha: optional_finite(&pass.constantshadervalues, "Alpha", 1.0)?,
                    power,
                    scroll: [
                        optional_finite(&pass.constantshadervalues, "Scroll 1 X", 0.0)?,
                        optional_finite(&pass.constantshadervalues, "Scroll 1 Y", 0.0)?,
                    ],
                    blending,
                    texture: textures[0].clone(),
                },
                vec![textures[0].clone()],
            ))
        }
        "flowimage" if textures.len() == 2 => {
            ensure!(
                pass.combos.is_empty() && pass.usershadervalues.is_empty(),
                "unsupported flow combos or user values"
            );
            for name in pass.constantshadervalues.keys() {
                ensure!(
                    matches!(
                        name.as_str(),
                        "Alpha"
                            | "Bright"
                            | "Amount"
                            | "Power"
                            | "Speed"
                            | "Scroll 1 X"
                            | "Scroll 1 Y"
                    ),
                    "unsupported flow constant {name:?}"
                );
            }
            Ok((
                MaterialProfile::Flow {
                    brightness: optional_finite(&pass.constantshadervalues, "Bright", 1.0)?,
                    speed: finite(&pass.constantshadervalues, "Speed")?,
                    amount: finite(&pass.constantshadervalues, "Amount")?,
                    bias: flow_bias(project_root)?,
                    base: textures[0].clone(),
                    mask: textures[1].clone(),
                },
                textures,
            ))
        }
        "flowimage" => {
            ensure!(
                pass.combos.is_empty() && pass.usershadervalues.is_empty(),
                "unsupported flow combos or user values"
            );
            ensure!(
                textures.len() == 4,
                "unsupported flow texture count {}",
                textures.len()
            );
            for name in pass.constantshadervalues.keys() {
                ensure!(
                    matches!(
                        name.as_str(),
                        "Alpha"
                            | "Bright"
                            | "Amount"
                            | "Power"
                            | "Speed"
                            | "Speed0"
                            | "Speed1"
                            | "Speed2"
                            | "Speed3"
                    ),
                    "unsupported flow constant {name:?}"
                );
            }
            Ok((
                MaterialProfile::FlowLayers {
                    brightness: optional_finite(&pass.constantshadervalues, "Bright", 1.0)?,
                    amount: finite(&pass.constantshadervalues, "Amount")?,
                    speeds: [
                        finite(&pass.constantshadervalues, "Speed0")?,
                        finite(&pass.constantshadervalues, "Speed1")?,
                        finite(&pass.constantshadervalues, "Speed2")?,
                    ],
                    bias: flow_bias(project_root)?,
                    mask: textures[0].clone(),
                    layers: [
                        textures[1].clone(),
                        textures[2].clone(),
                        textures[3].clone(),
                    ],
                },
                textures,
            ))
        }
        "swayimage" => {
            ensure!(
                pass.combos.is_empty() && pass.usershadervalues.is_empty(),
                "unsupported sway combos or user values"
            );
            ensure!(
                textures.len() == 2,
                "unsupported sway texture count {}",
                textures.len()
            );
            for name in pass.constantshadervalues.keys() {
                ensure!(
                    matches!(
                        name.as_str(),
                        "Alpha"
                            | "Bright"
                            | "Amount"
                            | "Power"
                            | "Speed"
                            | "Scroll 1 X"
                            | "Scroll 1 Y"
                    ),
                    "unsupported sway constant {name:?}"
                );
            }
            Ok((
                MaterialProfile::Sway {
                    brightness: optional_finite(&pass.constantshadervalues, "Bright", 1.0)?,
                    speed: finite(&pass.constantshadervalues, "Speed")?,
                    amount: finite(&pass.constantshadervalues, "Amount")?,
                    base: textures[0].clone(),
                    mask: textures[1].clone(),
                },
                textures,
            ))
        }
        other => anyhow::bail!("unsupported scene shader {other:?}"),
    }
}

impl Scene {
    pub fn profile(&self) -> &SceneProfile {
        &self.profile
    }

    /// Render one opaque BGRA frame at the requested physical output size.
    pub fn render(&self, width: u32, height: u32, fit: Fit, time: f32) -> Result<Vec<u8>> {
        let bytes = frame_len(width, height)?;
        ensure!(time.is_finite() && time >= 0.0, "scene time must be finite");
        let mut pixels = vec![0; bytes];
        let layout = self.layout(width, height, fit)?;
        self.render_rows(&layout, width, time, 0, &mut pixels);
        Ok(pixels)
    }

    pub fn start_frame(self, width: u32, height: u32, fit: Fit) -> Result<SceneFrame> {
        frame_len(width, height)?;
        Ok(SceneFrame {
            scene: self,
            width,
            height,
            fit,
            started: std::time::Instant::now(),
        })
    }
}

impl SceneFrame {
    pub fn elapsed(&self) -> f32 {
        self.started.elapsed().as_secs_f32()
    }

    /// Render one frame across a bounded worker pool.
    ///
    /// The pool is capped at eight threads and at least one output row per
    /// thread. This keeps small validation frames single-threaded while giving
    /// full-screen animation enough parallelism for interactive playback.
    pub fn render_frame(&self, time: f32) -> Result<Vec<u8>> {
        let bytes = frame_len(self.width, self.height)?;
        ensure!(time.is_finite() && time >= 0.0, "scene time must be finite");
        let mut pixels = vec![0; bytes];
        let layout = self.scene.layout(self.width, self.height, self.fit)?;
        let threads = render_threads(self.height);
        let rows_per = self.height.div_ceil(threads as u32);
        let row_bytes = (self.width as usize)
            .checked_mul(4)
            .context("scene row stride overflow")?;
        let chunk_bytes = (rows_per as usize)
            .checked_mul(row_bytes)
            .context("scene thread chunk overflow")?;

        std::thread::scope(|scope| {
            let mut chunks = pixels.chunks_mut(chunk_bytes);
            for chunk_index in 0..threads {
                let Some(chunk) = chunks.next() else {
                    break;
                };
                let start_y = chunk_index as u32 * rows_per;
                scope.spawn(move || {
                    self.scene
                        .render_rows(&layout, self.width, time, start_y, chunk);
                });
            }
        });
        Ok(pixels)
    }
}

fn render_threads(height: u32) -> usize {
    let parallelism = std::thread::available_parallelism()
        .map(|parallelism| parallelism.get())
        .unwrap_or(4);
    parallelism.clamp(1, 8).min(height.max(1) as usize).max(1)
}

/// Fit one scene projection into the output, shared by the CPU and GPU paths.
pub(crate) fn scene_layout(
    projection: [u32; 2],
    width: u32,
    height: u32,
    fit: Fit,
) -> Result<SceneLayout> {
    let (scene_width, scene_height) = (f64::from(projection[0]), f64::from(projection[1]));
    let factor = match fit {
        Fit::Cover => (f64::from(width) / scene_width).max(f64::from(height) / scene_height),
        Fit::Contain => (f64::from(width) / scene_width).min(f64::from(height) / scene_height),
    };
    ensure!(
        factor.is_finite() && factor > 0.0,
        "invalid scene fit scale"
    );
    Ok(SceneLayout {
        factor,
        left: (f64::from(width) - scene_width * factor) / 2.0,
        top: (f64::from(height) - scene_height * factor) / 2.0,
    })
}

impl Scene {
    #[cfg(feature = "gui")]
    pub(crate) fn runtime_layers(&self) -> &[LayerRuntime] {
        &self.layers
    }

    fn layout(&self, width: u32, height: u32, fit: Fit) -> Result<SceneLayout> {
        scene_layout(self.profile.projection, width, height, fit)
    }

    fn render_rows(
        &self,
        layout: &SceneLayout,
        width: u32,
        time: f32,
        start_y: u32,
        pixels: &mut [u8],
    ) {
        for (row_index, row) in pixels.chunks_exact_mut(width as usize * 4).enumerate() {
            let y = start_y + row_index as u32;
            for (x, pixel) in row.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                let mut color = [
                    self.profile.clear_color[0],
                    self.profile.clear_color[1],
                    self.profile.clear_color[2],
                    1.0,
                ];
                // Layers composite back-to-front in scene order.
                for layer in &self.layers {
                    let (u, v, inside) = Self::layer_uv(layout, &layer.geometry, x as u32, y);
                    if !inside {
                        continue;
                    }
                    color = composite(
                        color,
                        layer.material.sample(u, v, time),
                        layer.material.blending(),
                    );
                }
                pixel.copy_from_slice(&[
                    channel(color[2]),
                    channel(color[1]),
                    channel(color[0]),
                    255,
                ]);
            }
        }
    }

    fn layer_uv(
        layout: &SceneLayout,
        geometry: &LayerGeometry,
        x: u32,
        y: u32,
    ) -> (f32, f32, bool) {
        let scene_x = (f64::from(x) + 0.5 - layout.left) / layout.factor;
        let scene_y = (f64::from(y) + 0.5 - layout.top) / layout.factor;
        let dx = scene_x - f64::from(geometry.center[0]);
        let dy = scene_y - f64::from(geometry.center[1]);
        // In-plane rotation about the quad center; unrotated quads take the
        // exact previous path. The sign follows preview-verified orientation.
        let (u, v) = if geometry.rotation == 0.0 {
            (
                dx / f64::from(geometry.extents[0]) + 0.5,
                dy / f64::from(geometry.extents[1]) + 0.5,
            )
        } else {
            let (sin, cos) = (f64::from(geometry.rotation)).sin_cos();
            (
                (dx * cos + dy * sin) / f64::from(geometry.extents[0]) + 0.5,
                (dy * cos - dx * sin) / f64::from(geometry.extents[1]) + 0.5,
            )
        };
        let (u, v) = (u as f32, v as f32);
        (u, v, (0.0..=1.0).contains(&u) && (0.0..=1.0).contains(&v))
    }

    fn flag_rgb(material: &MaterialRuntime, u: f32, v: f32, time: f32) -> [f32; 3] {
        let MaterialRuntime::Flag {
            colors,
            speed,
            strength,
            base,
            normal,
            cloth,
        } = material
        else {
            return [0.0; 3];
        };
        // Direct transcription of flag.vert's scrolling normal coordinates and
        // flag.frag's tint, cloth, and lighting terms.
        let wave1 = u * 0.7 - time * speed;
        let wave2 = u * 0.3 - time * speed * 0.5;
        let mut n1x = wave1 - ((0.5 - u) * (1.0 - v)) * 3.0;
        n1x += 2.0 * signed_cubed(v - 0.1) * u * u;
        let n1y = v * 0.21;
        let mut n2x = wave2 - ((1.0 - u) * (1.0 - v)) * 2.0;
        let _ = &mut n2x;
        let n2y = v * 0.21;
        let n1 = decompress_normal(normal.sample_repeat(n1x, n1y));
        let n2 = decompress_normal(normal.sample_repeat(n2x, n2y));
        let mut normal = [n1[0] * n2[0], n1[1] * n2[1], n1[2] * n2[2]];
        normal = mix3([0.0, 0.0, 1.0], normal, *strength);
        normal = normalize(normal);

        let sample_u = u + normal[0] * 0.02;
        let sample_v = v + normal[1] * 0.02;
        let albedo = base.sample_clamp(sample_u, sample_v);
        let cloth = cloth.sample_repeat(sample_u * 4.0, sample_v * 4.0)[0];
        let mut color = mix3(colors.background, colors.accent1, albedo[0]);
        color = mix3(color, colors.accent2, albedo[1]);
        let cloth_light = albedo[2] * cloth;
        color = [
            color[0] * cloth_light + cloth * 0.1,
            color[1] * cloth_light + cloth * 0.1,
            color[2] * cloth_light + cloth * 0.1,
        ];
        let mut light = 0.7 + (normal[0] + normal[1]) * 0.353_553_39;
        light += light.powi(5) * 0.5;
        let cloth_boost = light * (0.0f32.max(cloth * 2.0 - 1.0));
        let total = light + cloth_boost;
        [color[0] * total, color[1] * total, color[2] * total]
    }
}

impl MaterialRuntime {
    pub(crate) fn blending(&self) -> Blending {
        match self {
            MaterialRuntime::Flag { .. } => Blending::Opaque,
            MaterialRuntime::Generic { blending, .. } => *blending,
            // The shipped flow/sway shaders write opaque alpha; their layers
            // composite as translucent content over the backdrop.
            MaterialRuntime::Flow { .. }
            | MaterialRuntime::FlowLayers { .. }
            | MaterialRuntime::Sway { .. } => Blending::Translucent,
        }
    }

    fn sample(&self, u: f32, v: f32, time: f32) -> [f32; 4] {
        match self {
            material @ MaterialRuntime::Flag { .. } => {
                let rgb = Scene::flag_rgb(material, u, v, time);
                [rgb[0], rgb[1], rgb[2], 1.0]
            }
            MaterialRuntime::Generic {
                brightness,
                alpha,
                power,
                scroll,
                tex,
                ..
            } => {
                // Signed-square scroll preserves continuity through zero; the
                // shipped GLSL pow() on negative bases is undefined behavior.
                let scroll = runtime_scroll(*scroll);
                let sample = tex.sample_repeat(u + time * scroll[0], v + time * scroll[1]);
                if *power == 1.0 {
                    [
                        sample[0] * brightness,
                        sample[1] * brightness,
                        sample[2] * brightness,
                        sample[3] * alpha,
                    ]
                } else {
                    [
                        (sample[0] * brightness).powf(*power),
                        (sample[1] * brightness).powf(*power),
                        (sample[2] * brightness).powf(*power),
                        sample[3] * alpha,
                    ]
                }
            }
            MaterialRuntime::Flow {
                brightness,
                speed,
                amount,
                bias,
                base,
                mask,
            } => {
                let flow = flow_offset(mask, u, v, *bias, *amount);
                let c1 = fract(time * speed);
                let c2 = fract(time * speed + 0.5);
                let blend = 2.0 * (c1 - 0.5).abs();
                let a = base.sample_repeat(u + flow[0] * c1, v + flow[1] * c1);
                let b = base.sample_repeat(u + flow[0] * c2, v + flow[1] * c2);
                let rgb = mix4(a, b, blend);
                [
                    rgb[0] * brightness,
                    rgb[1] * brightness,
                    rgb[2] * brightness,
                    rgb[3],
                ]
            }
            MaterialRuntime::FlowLayers {
                brightness,
                amount,
                speeds,
                bias,
                mask,
                layers,
            } => {
                let flow = flow_offset(mask, u, v, *bias, *amount);
                let mut albedo = [0.0, 0.0, 0.0, 0.0];
                let offsets = [0.0, 0.0, 0.3333];
                for (index, layer) in layers.iter().enumerate() {
                    let speed = speeds[index];
                    let c1 = fract(time * speed + offsets[index]);
                    let c2 = fract(time * speed + 0.5 + offsets[index]);
                    let blend = 2.0 * (c1 - 0.5).abs();
                    let a = layer.sample_repeat(u + flow[0] * c1, v + flow[1] * c1);
                    let b = layer.sample_repeat(u + flow[0] * c2, v + flow[1] * c2);
                    let sample = mix4(a, b, blend);
                    // blendLayers: rgb mixes by sample alpha, alpha takes the max.
                    albedo[0] = albedo[0] + (sample[0] - albedo[0]) * sample[3];
                    albedo[1] = albedo[1] + (sample[1] - albedo[1]) * sample[3];
                    albedo[2] = albedo[2] + (sample[2] - albedo[2]) * sample[3];
                    albedo[3] = albedo[3].max(sample[3]);
                }
                [
                    albedo[0] * brightness,
                    albedo[1] * brightness,
                    albedo[2] * brightness,
                    albedo[3],
                ]
            }
            MaterialRuntime::Sway {
                brightness,
                speed,
                amount,
                base,
                mask,
            } => {
                let sway = mask.sample_repeat(u, v);
                let phase = u * 10.0 + sway[2];
                let t = time * speed;
                let amt = (t * 30.0 + phase).sin()
                    + (t * 27.0 + phase).sin()
                    + (t * 21.0 + phase).sin()
                    + (t * 7.0 + phase).sin();
                let offset = [sway[0] * amt * amount * 0.01, sway[1] * amt * amount * 0.01];
                let albedo = base.sample_repeat(u + offset[0], v + offset[1]);
                [
                    albedo[0] * brightness,
                    albedo[1] * brightness,
                    albedo[2] * brightness,
                    albedo[3],
                ]
            }
        }
    }
}

/// Flow distortion vector before per-cycle scaling: (mask.rg - bias) * 2.
/// The bias literal comes from the project's own flowimage.frag source.
fn flow_offset(mask: &Texture, u: f32, v: f32, bias: [f32; 2], amount: f32) -> [f32; 2] {
    let sample = mask.sample_repeat(u, v);
    [
        (sample[0] - bias[0]) * 2.0 * amount * 0.1,
        (sample[1] - bias[1]) * 2.0 * amount * 0.1,
    ]
}

/// Mask bias extracted from the project's own `shaders/flowimage.frag`.
/// Comment lines are ignored; exactly one flow bias literal must remain.
fn flow_bias(project_root: &Path) -> Result<[f32; 2]> {
    let path = root_relative(project_root, "shaders/flowimage.frag")?;
    let mut source = String::new();
    open_regular_file(&path)?
        .take(65_536 + 1)
        .read_to_string(&mut source)?;
    ensure!(
        source.len() <= 65_536,
        "project flow shader exceeds the 64 KiB budget"
    );
    let mut found = Vec::new();
    for line in source.lines() {
        let code = line.split("//").next().unwrap_or("");
        if !(code.contains("flowMask") || code.contains("flowColors")) {
            continue;
        }
        let mut rest = code;
        while let Some(start) = rest.find("vec2(") {
            rest = &rest[start + 5..];
            let Some(end) = rest.find(')') else {
                break;
            };
            let pair = &rest[..end];
            rest = &rest[end + 1..];
            let mut parts = pair.split(',');
            let (Some(x), Some(y), None) = (parts.next(), parts.next(), parts.next()) else {
                continue;
            };
            let (Ok(x), Ok(y)) = (x.trim().parse::<f32>(), y.trim().parse::<f32>()) else {
                continue;
            };
            if x.is_finite() && y.is_finite() {
                found.push([x, y]);
            }
        }
    }
    ensure!(
        found.len() == 1,
        "project flow shader must declare exactly one flow bias, found {}",
        found.len()
    );
    Ok(found[0])
}

fn fract(value: f32) -> f32 {
    value - value.floor()
}

/// Signed-square scroll preserves continuity through zero; the shipped GLSL
/// pow() on negative bases is undefined behavior. Shared with the GPU path.
pub(crate) fn runtime_scroll(scroll: [f32; 2]) -> [f32; 2] {
    [
        scroll[0].signum() * scroll[0] * scroll[0],
        scroll[1].signum() * scroll[1] * scroll[1],
    ]
}

fn mix4(a: [f32; 4], b: [f32; 4], t: f32) -> [f32; 4] {
    std::array::from_fn(|index| a[index] + (b[index] - a[index]) * t)
}

fn read_json<T>(path: &Path, limit: u64) -> Result<T>
where
    T: for<'de> Deserialize<'de>,
{
    let mut bytes = Vec::new();
    open_regular_file(path)?
        .take(limit + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= limit,
        "{} exceeds the JSON budget",
        path.display()
    );
    serde_json::from_slice(&bytes).with_context(|| format!("invalid {}", path.display()))
}

fn root_relative(root: &Path, declaration: &str) -> Result<PathBuf> {
    let root = root
        .canonicalize()
        .context("cannot resolve scene project root")?;
    scene_relative(&root, &root, declaration)
}

fn scene_relative(parent: &Path, root: &Path, declaration: &str) -> Result<PathBuf> {
    let relative = Path::new(declaration);
    ensure!(
        !declaration.is_empty()
            && !declaration.contains('\\')
            && relative.components().all(|item| matches!(
                item,
                std::path::Component::Normal(_) | std::path::Component::CurDir
            )),
        "scene files must be relative paths without parent traversal or backslashes"
    );
    let root = root
        .canonicalize()
        .context("cannot resolve scene project root")?;
    let selected = parent
        .canonicalize()
        .and_then(|parent| parent.join(relative).canonicalize())
        .with_context(|| format!("cannot resolve scene file {declaration:?}"))?;
    ensure!(
        selected.starts_with(&root),
        "scene file resolves outside its project directory"
    );
    ensure!(selected.is_file(), "scene file is not a regular file");
    Ok(selected)
}

fn parse_triplet(value: &str) -> Result<[f32; 3]> {
    let mut parts = value.split_whitespace();
    let triplet = [
        parse_component(parts.next())?,
        parse_component(parts.next())?,
        parse_component(parts.next())?,
    ];
    ensure!(
        parts.next().is_none(),
        "vector {value:?} must contain exactly three numbers"
    );
    Ok(triplet)
}

fn parse_component(value: Option<&str>) -> Result<f32> {
    let value: f32 = value
        .context("vector must contain exactly three numbers")?
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid vector number"))?;
    ensure!(value.is_finite(), "vector numbers must be finite");
    Ok(value)
}

fn mix3(a: [f32; 3], b: [f32; 3], t: f32) -> [f32; 3] {
    std::array::from_fn(|index| a[index] + (b[index] - a[index]) * t)
}

fn normalize(v: [f32; 3]) -> [f32; 3] {
    let length = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
    if length > 0.0 {
        [v[0] / length, v[1] / length, v[2] / length]
    } else {
        [0.0, 0.0, 1.0]
    }
}

/// Mathematical cube, including for negative inputs.
///
/// GLSL `pow(x, 3.0)` is only defined for nonnegative `x`, but the shipped flag
/// expression is continuous through `v == 0.1` in the static preview. Preserve
/// that observed behavior rather than emitting NaNs along the flag.
fn signed_cubed(value: f32) -> f32 {
    value * value * value
}

fn decompress_normal(sample: [f32; 4]) -> [f32; 3] {
    // DXT5 branch of the shipped DecompressNormal helper.
    let x = sample[1] * 2.0 - 0.965;
    let y = sample[3] * 2.0 - 1.0;
    let z = (0.0f32.max(1.0 - x * x - y * y)).sqrt();
    [x, y, z]
}

fn channel(value: f32) -> u8 {
    (value * 255.0).round().clamp(0.0, 255.0) as u8
}

/// Extract the color values of a project's declared properties. Non-color
/// editable properties (slider, combo, bool, texture, checkbox) and properties
/// without a type are tolerated and ignored; malformed color values still fail.
pub fn parse_project_colors(
    properties: &HashMap<String, serde_json::Value>,
) -> Result<HashMap<String, [f32; 3]>> {
    let mut colors = HashMap::new();
    for (name, property) in properties {
        let Some(object) = property.as_object() else {
            crate::log_debug!("scene project property {name:?} is not an object; ignored");
            continue;
        };
        // Legacy projects may omit the type; a parseable triplet is still a color.
        let kind = object
            .get("type")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("color");
        if !kind.eq_ignore_ascii_case("color") {
            crate::log_debug!(
                "scene project property {name:?} of type {kind:?} is not a color; ignored"
            );
            continue;
        }
        let value = object
            .get("value")
            .and_then(serde_json::Value::as_str)
            .with_context(|| format!("scene project color {name:?} value is missing"))?;
        colors.insert(name.clone(), parse_triplet(value)?);
    }
    Ok(colors)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(root: &Path) {
        let materials = root.join("materials");
        let models = root.join("models");
        std::fs::create_dir_all(&materials).unwrap();
        std::fs::create_dir_all(&models).unwrap();
        std::fs::write(
            root.join("project.json"),
            r#"{"type":"scene","file":"scene.json","general":{"properties":{"schemecolor":{"type":"color","value":"0 0 0"},"flagcolor1":{"type":"color","value":"1 1 1"},"flagcolor2":{"type":"color","value":"1 0 0"}}}}"#,
        )
        .unwrap();
        std::fs::write(
            root.join("scene.json"),
                r#"{"camera":{"center":"0 0 -1","eye":"0 0 0","up":"0 1 0"},"general":{"bloom":false,"clearcolor":"0.1 0.2 0.3","orthogonalprojection":{"width":6,"height":2}},"objects":[{"angles":"0 0 0","id":7,"image":"models/quad.json","name":"quad","origin":"3 1 0","scale":"1 1 1"}]}"#,
        )
        .unwrap();
        std::fs::write(
            models.join("quad.json"),
            r#"{"height":2,"material":"materials/flag.json","width":6}"#,
        )
        .unwrap();
        std::fs::write(
            materials.join("flag.json"),
            r#"{"passes":[{"combos":{"TINT":1},"constantshadervalues":{"Speed":0.5,"Strength":0.5},"depthtest":"disabled","depthwrite":"disabled","shader":"flag","textures":["base","normal","cloth"],"usershadervalues":{"flagcolor1":"color2","flagcolor2":"color3","schemecolor":"color1"}}]}"#,
        )
        .unwrap();
        for (stem, bytes) in [
            ("base", [255, 0, 0, 255, 0, 255, 0, 255].repeat(6)),
            ("normal", [0, 0, 0, 0, 0, 255, 0, 255].repeat(6)),
            ("cloth", [255; 48].to_vec()),
        ] {
            let mut file = Vec::from(b"TEXV0005\0TEXI0001\0".as_slice());
            for word in [0u32, 0, 6, 2, 6, 2, 0] {
                file.extend_from_slice(&word.to_le_bytes());
            }
            file.extend_from_slice(b"TEXB0003\0");
            for word in [1u32, u32::MAX, 1, 6u32, 2, 0, 48, 48] {
                file.extend_from_slice(&word.to_le_bytes());
            }
            file.extend_from_slice(&bytes);
            std::fs::write(materials.join(format!("{stem}.tex")), file).unwrap();
        }
    }

    fn properties(root: &Path) -> HashMap<String, [f32; 3]> {
        let manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(root.join("project.json")).unwrap()).unwrap();
        let properties: HashMap<String, serde_json::Value> =
            serde_json::from_value(manifest["general"]["properties"].clone()).unwrap();
        parse_project_colors(&properties).unwrap()
    }

    #[test]
    fn accepts_one_exact_flag_profile_without_preview_substitution() {
        let root = tempfile::tempdir().unwrap();
        fixture(root.path());
        let scene = load(
            &root.path().join("scene.json"),
            root.path(),
            &properties(root.path()),
        )
        .unwrap();
        assert_eq!(scene.profile.projection, [6, 2]);
        assert_eq!(scene.profile.layers.len(), 1);
        assert_eq!(scene.profile.layers[0].extents, [6.0, 2.0]);
        let first = scene.render(6, 2, Fit::Cover, 0.0).unwrap();
        let later = scene.render(6, 2, Fit::Cover, 0.37).unwrap();
        assert_eq!(first.len(), 48);
        assert_ne!(first, later);
        let frame = scene.start_frame(6, 2, Fit::Cover).unwrap();
        assert_eq!(frame.render_frame(0.37).unwrap(), later);
        assert!(first.as_chunks::<4>().0.iter().all(|pixel| pixel[3] == 255));
    }

    #[test]
    fn tolerates_unsupported_features_and_skips_only_unrenderable_objects() {
        let root = tempfile::tempdir().unwrap();
        fixture(root.path());
        let scene_path = root.path().join("scene.json");
        let original = std::fs::read(&scene_path).unwrap();
        let mutate = |mutate: &dyn Fn(&mut serde_json::Value)| {
            std::fs::write(&scene_path, &original).unwrap();
            let mut document: serde_json::Value = serde_json::from_slice(&original).unwrap();
            mutate(&mut document);
            std::fs::write(&scene_path, serde_json::to_vec(&document).unwrap()).unwrap();
            load(&scene_path, root.path(), &properties(root.path()))
        };
        // An unsupported single object leaves nothing renderable; the reason survives.
        let error = mutate(&|document| {
            document["objects"][0]["angles"] = serde_json::Value::String("0 1 0".into())
        })
        .unwrap_err();
        assert!(format!("{error:#}").contains("rotation"));
        // Bloom and unknown fields are tolerated and reported instead of failing.
        let scene =
            mutate(&|document| document["general"]["bloom"] = serde_json::Value::Bool(true))
                .unwrap();
        assert!(scene
            .profile
            .limitations
            .iter()
            .any(|limitation| limitation.contains("Bloom")));
        let error = mutate(&|document| {
            document["objects"][0]["image"] =
                serde_json::Value::String("models/missing.json".into())
        })
        .unwrap_err();
        assert!(format!("{error:#}").contains("invalid scene model"));
        let scene = mutate(&|document| {
            document["objects"][0]["parallaxDepth"] = serde_json::json!(0.25);
            document["general"]["hdr"] = serde_json::json!(true);
            document["camera"]["paths"] = serde_json::json!(["scripts/camera_00.json"]);
        })
        .unwrap();
        let limitations = scene.profile.limitations.join("\n");
        assert!(limitations.contains("parallaxDepth"));
        assert!(limitations.contains("hdr"));
        assert!(limitations.contains("Camera path scripts were not applied"));
        assert_eq!(scene.profile.layers.len(), 1);
        // An invisible object is omitted by intent, not reported as unsupported.
        let scene =
            mutate(&|document| document["objects"][0]["visible"] = serde_json::Value::Bool(false))
                .unwrap();
        assert_eq!(scene.profile.layers.len(), 0);
        assert!(scene
            .profile
            .limitations
            .iter()
            .any(|limitation| limitation.contains("invisible")));
    }

    #[test]
    fn skips_3d_models_particles_and_geometry_less_objects() {
        let root = tempfile::tempdir().unwrap();
        layered_fixture(root.path());
        let scene_path = root.path().join("scene.json");
        let mut document: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&scene_path).unwrap()).unwrap();
        let objects = document["objects"].as_array_mut().unwrap();
        objects.push(serde_json::json!({"angles":"0 0 0","id":3,"name":"cube","model":"models/cube.mdl","origin":"3 1 0","scale":"1 1 1"}));
        objects.push(serde_json::json!({"id":4,"name":"sparks","origin":"3 1 0","particlesrc":"particles/sparks.json"}));
        objects.push(
            serde_json::json!({"id":5,"name":"mystery","origin":"3 1 0","somethingnew":true}),
        );
        std::fs::write(&scene_path, serde_json::to_vec(&document).unwrap()).unwrap();
        let scene = load(&scene_path, root.path(), &HashMap::new()).unwrap();
        assert_eq!(scene.profile.layers.len(), 2);
        let limitations = scene.profile.limitations.join("\n");
        assert!(
            limitations.contains("Renders 2 of 5 objects"),
            "{limitations}"
        );
        assert!(limitations.contains("1 3D models"), "{limitations}");
        assert!(limitations.contains("1 particle systems"), "{limitations}");
        assert!(
            limitations.contains("1 objects without image or model"),
            "{limitations}"
        );
    }

    fn tex_bytes(pixels: Vec<u8>) -> Vec<u8> {
        // Wrap 6x2 RGBA pixels in a minimal raw TEXV0005 container.
        let mut file = Vec::from(b"TEXV0005\0TEXI0001\0".as_slice());
        for word in [0u32, 0, 6, 2, 6, 2, 0] {
            file.extend_from_slice(&word.to_le_bytes());
        }
        file.extend_from_slice(b"TEXB0003\0");
        for word in [1u32, u32::MAX, 1, 6u32, 2, 0, 48, 48] {
            file.extend_from_slice(&word.to_le_bytes());
        }
        file.extend_from_slice(&pixels);
        file
    }

    fn layered_fixture(root: &Path) {
        let materials = root.join("materials");
        let models = root.join("models");
        std::fs::create_dir_all(&materials).unwrap();
        std::fs::create_dir_all(&models).unwrap();
        std::fs::write(
            root.join("project.json"),
            r#"{"type":"scene","file":"scene.json"}"#,
        )
        .unwrap();
        std::fs::write(
            root.join("scene.json"),
            r#"{"camera":{"center":"0 0 -1","eye":"0 0 0","up":"0 1 0"},"general":{"bloom":false,"clearcolor":"0 0 0","orthogonalprojection":{"width":6,"height":2}},"objects":[
                {"angles":"0 0 0","id":1,"image":"models/back.json","name":"back","origin":"3 1 0","scale":"1 1 1"},
                {"angles":"0 0 0","id":2,"image":"models/front.json","name":"front","origin":"3 1 0","scale":"1 1 1"}]}"#,
        )
        .unwrap();
        std::fs::write(
            models.join("back.json"),
            r#"{"height":2,"material":"materials/plain.json","width":6}"#,
        )
        .unwrap();
        std::fs::write(
            models.join("front.json"),
            r#"{"height":2,"material":"materials/glass.json","width":6}"#,
        )
        .unwrap();
        std::fs::write(
            materials.join("plain.json"),
            r#"{"passes":[{"depthtest":"disabled","depthwrite":"disabled","shader":"genericimage","textures":["back"]}]}"#,
        )
        .unwrap();
        std::fs::write(
            materials.join("glass.json"),
            r#"{"passes":[{"blending":"translucent","depthtest":"disabled","depthwrite":"disabled","shader":"genericimage","textures":["front"],"constantshadervalues":{"Alpha":0.5}}]}"#,
        )
        .unwrap();
        // Opaque red backdrop, half-transparent green foreground.
        std::fs::write(
            materials.join("back.tex"),
            tex_bytes([255, 0, 0, 255].repeat(12)),
        )
        .unwrap();
        std::fs::write(
            materials.join("front.tex"),
            tex_bytes([0, 255, 0, 255].repeat(12)),
        )
        .unwrap();
    }

    #[test]
    fn layers_composite_back_to_front_with_alpha() {
        let root = tempfile::tempdir().unwrap();
        layered_fixture(root.path());
        let scene = load(
            &root.path().join("scene.json"),
            root.path(),
            &HashMap::new(),
        )
        .unwrap();
        assert_eq!(scene.profile.layers.len(), 2);
        let frame = scene.render(6, 2, Fit::Cover, 0.0).unwrap();
        // Red overlaid with 50% green: (128, 128, 0) after rounding.
        assert_eq!(&frame[0..4], &[0, 128, 128, 255]);
        assert_eq!(frame.len(), 48);
    }

    #[test]
    fn unsupported_materials_skip_their_object_with_the_reason_preserved() {
        let root = tempfile::tempdir().unwrap();
        layered_fixture(root.path());
        let material = root.path().join("materials/plain.json");
        let mutate = |text: &str| {
            std::fs::write(&material, text).unwrap();
            let scene = load(
                &root.path().join("scene.json"),
                root.path(),
                &HashMap::new(),
            )
            .unwrap();
            assert_eq!(
                scene.profile.layers.len(),
                1,
                "only the front object remains"
            );
            scene.profile.limitations.join("\n")
        };
        assert!(mutate(r#"{"passes":[{"depthtest":"disabled","depthwrite":"disabled","shader":"nope","textures":["back"]}]}"#)
            .contains("unsupported scene shader \"nope\""));
        assert!(mutate(r#"{"passes":[{"combos":{"spritesheet":1},"depthtest":"disabled","depthwrite":"disabled","shader":"genericimage","textures":["back"]}]}"#)
            .contains("unsupported generic combo \"spritesheet\""));
        assert!(mutate(r#"{"passes":[{"blending":"multiply","depthtest":"disabled","depthwrite":"disabled","shader":"genericimage","textures":["back"]}]}"#)
            .contains("unsupported blending"));
        assert!(mutate(r#"{"passes":[{"depthtest":"disabled","depthwrite":"disabled","shader":"genericimage","textures":["back"],"constantshadervalues":{"Power":-1}}]}"#)
            .contains("nonnegative"));
        // With every object skipped, loading fails closed and still reports why.
        std::fs::write(
            root.path().join("materials/glass.json"),
            r#"{"passes":[{"depthtest":"disabled","depthwrite":"disabled","shader":"nope","textures":["front"]}]}"#,
        )
        .unwrap();
        let error = load(
            &root.path().join("scene.json"),
            root.path(),
            &HashMap::new(),
        )
        .unwrap_err();
        let message = format!("{error:#}");
        assert!(
            message.contains("unsupported scene shader \"nope\""),
            "{message}"
        );
        assert!(message.contains("nonnegative"), "{message}");
    }
}
