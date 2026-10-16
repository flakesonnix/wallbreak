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
    collections::HashMap,
    fs::File,
    io::Read,
    path::{Path, PathBuf},
};

use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};

use crate::{
    media::{frame_len, Fit},
    texture::{load as load_texture, Texture},
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
struct LayerRuntime {
    geometry: LayerGeometry,
    material: MaterialRuntime,
}

impl MaterialProfile {
    fn texture_count(&self) -> usize {
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
enum MaterialRuntime {
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
struct SceneLayout {
    factor: f64,
    left: f64,
    top: f64,
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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SceneDocument {
    camera: CameraDocument,
    general: GeneralDocument,
    objects: Vec<ObjectDocument>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CameraDocument {
    center: String,
    eye: String,
    up: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GeneralDocument {
    ambientcolor: Option<String>,
    bloom: bool,
    clearcolor: String,
    norecompile: Option<bool>,
    orthogonalprojection: ProjectionDocument,
    skylightcolor: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProjectionDocument {
    height: u32,
    width: u32,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ObjectDocument {
    angles: String,
    id: u32,
    image: String,
    name: String,
    origin: String,
    scale: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelDocument {
    height: u32,
    material: String,
    width: u32,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MaterialDocument {
    passes: Vec<PassDocument>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PassDocument {
    #[serde(default)]
    combos: HashMap<String, u8>,
    #[serde(default)]
    constantshadervalues: HashMap<String, f32>,
    depthtest: String,
    depthwrite: String,
    shader: String,
    textures: Vec<String>,
    #[serde(default)]
    usershadervalues: HashMap<String, String>,
    blending: Option<String>,
    cullmode: Option<String>,
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
    let spec = parse(scene_path, project_root, properties)?;
    let mut textures = Vec::with_capacity(spec.texture_paths.len());
    let mut rgba_total = 0usize;
    for path in &spec.texture_paths {
        let texture = load_texture(path)
            .with_context(|| format!("invalid native texture {}", path.display()))?;
        rgba_total = rgba_total
            .checked_add(texture.rgba.len())
            .context("scene texture total overflow")?;
        ensure!(
            rgba_total <= MAX_TEXTURE_RGBA_TOTAL,
            "scene textures exceed 64 MiB decoded budget"
        );
        textures.push(texture);
    }
    let mut textures = textures.into_iter();
    let mut layers = Vec::with_capacity(spec.profile.layers.len());
    for layer in &spec.profile.layers {
        let count = layer.material.texture_count();
        let mut owned = Vec::with_capacity(count);
        for _ in 0..count {
            owned.push(
                textures
                    .next()
                    .context("scene texture budget miscounted its layers")?,
            );
        }
        layers.push(LayerRuntime {
            geometry: LayerGeometry {
                center: layer.center,
                extents: layer.extents,
                rotation: layer.rotation,
            },
            material: MaterialRuntime::bind(&layer.material, owned)?,
        });
    }
    Ok(Scene {
        profile: spec.profile,
        layers,
    })
}

/// Validate one layered scene project selection without decoding textures.
///
/// Used to mark Steam projects as supported before the user selects one.
pub fn parse(
    scene_path: &Path,
    project_root: &Path,
    properties: &HashMap<String, [f32; 3]>,
) -> Result<SceneSpec> {
    let document: SceneDocument =
        read_json(scene_path, MAX_SCENE_BYTES).context("invalid scene.json")?;
    ensure!(!document.objects.is_empty(), "scene has no image objects");
    let camera = Camera {
        eye: parse_triplet(&document.camera.eye).context("invalid camera eye")?,
        center: parse_triplet(&document.camera.center).context("invalid camera center")?,
        up: parse_triplet(&document.camera.up).context("invalid camera up")?,
    };
    ensure!(!document.general.bloom, "unsupported bloom-enabled scene");
    let projection = [
        document.general.orthogonalprojection.width,
        document.general.orthogonalprojection.height,
    ];
    ensure!(
        projection[0] > 0 && projection[1] > 0,
        "scene projection must be nonzero"
    );
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
    let mut layers = Vec::with_capacity(document.objects.len());
    let mut texture_paths = Vec::new();
    for object in &document.objects {
        let (layer, paths) = parse_layer(object, project_root, properties)?;
        texture_paths.extend(paths);
        layers.push(layer);
    }
    let profile = SceneProfile {
        scene_path: scene_path.to_path_buf(),
        projection,
        clear_color,
        camera,
        layers,
        limitations: vec![
            "Renders orthographic image quads back-to-front with flag, generic, flow, and sway materials; camera eye/center metadata are retained, not used for placement.".into(),
            "Uses native TEX textures only; embedded PNG files are not substituted.".into(),
            "CPU reimplementation of the shipped vertex/fragment behavior; no engine code is executed.".into(),
        ],
    };
    Ok(SceneSpec {
        profile,
        texture_paths,
    })
}

fn parse_layer(
    object: &ObjectDocument,
    project_root: &Path,
    properties: &HashMap<String, [f32; 3]>,
) -> Result<(LayerProfile, Vec<PathBuf>)> {
    let angles = parse_triplet(&object.angles).context("invalid object angles")?;
    ensure!(
        angles[0] == 0.0 && angles[1] == 0.0 && angles[2].is_finite(),
        "unsupported object rotation {angles:?}; only in-plane rotation is supported"
    );
    let rotation = angles[2];
    let origin = parse_triplet(&object.origin).context("invalid object origin")?;
    let scale = parse_triplet(&object.scale).context("invalid object scale")?;
    ensure!(
        scale[0].is_finite()
            && scale[1].is_finite()
            && scale[0] > 0.0
            && scale[1] > 0.0
            && scale[2] == 1.0,
        "unsupported object scale {scale:?}"
    );
    let model_path = root_relative(project_root, &object.image).context("invalid scene model")?;
    let model: ModelDocument =
        read_json(&model_path, MAX_JSON_BYTES).context("invalid scene model")?;
    ensure!(
        model.width > 0 && model.height > 0,
        "scene model dimensions must be nonzero"
    );
    let material_path =
        root_relative(project_root, &model.material).context("invalid scene material")?;
    let material: MaterialDocument =
        read_json(&material_path, MAX_JSON_BYTES).context("invalid scene material")?;
    ensure!(
        material.passes.len() == 1,
        "unsupported object with {} material passes",
        material.passes.len()
    );
    let material_root = material_path.parent().context("material has no parent")?;
    let project_root = project_root
        .canonicalize()
        .context("cannot resolve scene project root")?;
    let (profile, stems) = parse_material(&material.passes[0], &project_root, properties)?;
    let mut paths = Vec::with_capacity(stems.len());
    for stem in &stems {
        paths.push(scene_relative(
            material_root,
            &project_root,
            &format!("{stem}.tex"),
        )?);
    }
    Ok((
        LayerProfile {
            id: object.id,
            name: object.name.clone(),
            center: [origin[0], origin[1]],
            extents: [
                (model.width as f32) * scale[0],
                (model.height as f32) * scale[1],
            ],
            rotation,
            model: [model.width, model.height],
            material: profile,
        },
        paths,
    ))
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
) -> Result<(MaterialProfile, Vec<String>)> {
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
                        pass.textures[0].clone(),
                        pass.textures[1].clone(),
                        pass.textures[2].clone(),
                    ],
                },
                pass.textures.clone(),
            ))
        }
        "genericimage" | "genericimage2" => {
            ensure!(
                pass.combos.is_empty(),
                "unsupported generic combos {:?}; only default shading is supported",
                pass.combos
            );
            ensure!(
                pass.usershadervalues.is_empty(),
                "unsupported generic user shader values"
            );
            ensure!(
                pass.textures.len() == 1,
                "unsupported generic texture count {}",
                pass.textures.len()
            );
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
                    texture: pass.textures[0].clone(),
                },
                pass.textures.clone(),
            ))
        }
        "flowimage" if pass.textures.len() == 2 => {
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
                    base: pass.textures[0].clone(),
                    mask: pass.textures[1].clone(),
                },
                pass.textures.clone(),
            ))
        }
        "flowimage" => {
            ensure!(
                pass.combos.is_empty() && pass.usershadervalues.is_empty(),
                "unsupported flow combos or user values"
            );
            ensure!(
                pass.textures.len() == 4,
                "unsupported flow texture count {}",
                pass.textures.len()
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
                    mask: pass.textures[0].clone(),
                    layers: [
                        pass.textures[1].clone(),
                        pass.textures[2].clone(),
                        pass.textures[3].clone(),
                    ],
                },
                pass.textures.clone(),
            ))
        }
        "swayimage" => {
            ensure!(
                pass.combos.is_empty() && pass.usershadervalues.is_empty(),
                "unsupported sway combos or user values"
            );
            ensure!(
                pass.textures.len() == 2,
                "unsupported sway texture count {}",
                pass.textures.len()
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
                    base: pass.textures[0].clone(),
                    mask: pass.textures[1].clone(),
                },
                pass.textures.clone(),
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

impl Scene {
    fn layout(&self, width: u32, height: u32, fit: Fit) -> Result<SceneLayout> {
        let (scene_width, scene_height) = (
            f64::from(self.profile.projection[0]),
            f64::from(self.profile.projection[1]),
        );
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
    fn blending(&self) -> Blending {
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
                let scroll = [
                    scroll[0].signum() * scroll[0] * scroll[0],
                    scroll[1].signum() * scroll[1] * scroll[1],
                ];
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
    let path = project_root.join("shaders/flowimage.frag");
    let source =
        std::fs::read_to_string(&path).with_context(|| format!("missing {}", path.display()))?;
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

fn mix4(a: [f32; 4], b: [f32; 4], t: f32) -> [f32; 4] {
    std::array::from_fn(|index| a[index] + (b[index] - a[index]) * t)
}

fn read_json<T>(path: &Path, limit: u64) -> Result<T>
where
    T: for<'de> Deserialize<'de>,
{
    let mut bytes = Vec::new();
    File::open(path)
        .with_context(|| format!("cannot open {}", path.display()))?
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

pub fn parse_project_colors(
    properties: &HashMap<String, serde_json::Value>,
) -> Result<HashMap<String, [f32; 3]>> {
    let mut colors = HashMap::new();
    for (name, property) in properties {
        let Some(object) = property.as_object() else {
            bail!("scene project property {name:?} must be an object")
        };
        let kind = object
            .get("type")
            .and_then(serde_json::Value::as_str)
            .context("scene project property type is missing")?;
        ensure!(
            kind == "color",
            "unsupported scene project property {name:?} of type {kind:?}"
        );
        let value = object
            .get("value")
            .and_then(serde_json::Value::as_str)
            .context("scene project color value is missing")?;
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
    fn rejects_unsupported_scene_features_before_textures() {
        let root = tempfile::tempdir().unwrap();
        fixture(root.path());
        let scene_path = root.path().join("scene.json");
        let mutate = |mutate: &dyn Fn(&mut serde_json::Value)| {
            let mut document: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&scene_path).unwrap()).unwrap();
            mutate(&mut document);
            std::fs::write(&scene_path, serde_json::to_vec(&document).unwrap()).unwrap();
            let error = load(&scene_path, root.path(), &properties(root.path())).unwrap_err();
            std::fs::write(
                &scene_path,
            r#"{"camera":{"center":"0 0 -1","eye":"0 0 0","up":"0 1 0"},"general":{"bloom":false,"clearcolor":"0.1 0.2 0.3","orthogonalprojection":{"width":6,"height":2}},"objects":[{"angles":"0 0 0","id":7,"image":"models/quad.json","name":"quad","origin":"3 1 0","scale":"1 1 1"}]}"#,
            )
            .unwrap();
            format!("{error:#}")
        };
        assert!(mutate(&|document| document["objects"][0]["angles"] =
            serde_json::Value::String("0 1 0".into()))
        .contains("rotation"));
        assert!(
            mutate(&|document| document["general"]["bloom"] = serde_json::Value::Bool(true))
                .contains("bloom")
        );
        assert!(mutate(&|document| document["objects"][0]["image"] =
            serde_json::Value::String("models/missing.json".into()))
        .contains("invalid scene model"));
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
    fn rejects_unknown_shaders_combos_and_blending() {
        let root = tempfile::tempdir().unwrap();
        layered_fixture(root.path());
        let material = root.path().join("materials/plain.json");
        let mutate = |text: &str| {
            std::fs::write(&material, text).unwrap();
            let error = load(
                &root.path().join("scene.json"),
                root.path(),
                &HashMap::new(),
            )
            .unwrap_err();
            format!("{error:#}")
        };
        assert!(
            mutate(r#"{"passes":[{"depthtest":"disabled","depthwrite":"disabled","shader":"nope","textures":["back"]}]}"#)
                .contains("unsupported scene shader")
        );
        assert!(
            mutate(r#"{"passes":[{"combos":{"spritesheet":1},"depthtest":"disabled","depthwrite":"disabled","shader":"genericimage","textures":["back"]}]}"#)
                .contains("unsupported generic combos")
        );
        assert!(
            mutate(r#"{"passes":[{"blending":"multiply","depthtest":"disabled","depthwrite":"disabled","shader":"genericimage","textures":["back"]}]}"#)
                .contains("unsupported blending")
        );
        assert!(
            mutate(r#"{"passes":[{"depthtest":"disabled","depthwrite":"disabled","shader":"genericimage","textures":["back"],"constantshadervalues":{"Power":-1}}]}"#)
                .contains("nonnegative")
        );
    }
}
