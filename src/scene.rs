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
const MAX_TEXTURE_RGBA_TOTAL: usize = 64 * 1024 * 1024;
const SUPPORTED_SHADER: &str = "flag";

#[derive(Debug, Clone, Serialize)]
pub struct SceneProfile {
    pub scene_path: PathBuf,
    pub projection: [u32; 2],
    pub clear_color: [f32; 3],
    pub camera: Camera,
    pub object: ObjectGeometry,
    pub colors: FlagColors,
    pub speed: f32,
    pub strength: f32,
    pub textures: [String; 3],
    pub limitations: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Camera {
    pub eye: [f32; 3],
    pub center: [f32; 3],
    pub up: [f32; 3],
}

#[derive(Debug, Clone, Serialize)]
pub struct ObjectGeometry {
    pub id: u32,
    pub name: String,
    pub center: [f32; 2],
    pub extents: [f32; 2],
    pub model: [u32; 2],
}

#[derive(Debug, Clone, Serialize)]
pub struct FlagColors {
    pub background: [f32; 3],
    pub accent1: [f32; 3],
    pub accent2: [f32; 3],
}

#[derive(Debug, Clone)]
pub struct Scene {
    profile: SceneProfile,
    base: Texture,
    normal: Texture,
    cloth: Texture,
}

#[derive(Debug, Clone, Copy)]
struct SceneLayout {
    factor: f64,
    left: f64,
    top: f64,
    left_edge: f64,
    top_edge: f64,
    extent_x: f64,
    extent_y: f64,
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
    combos: HashMap<String, u8>,
    constantshadervalues: HashMap<String, f32>,
    depthtest: String,
    depthwrite: String,
    shader: String,
    textures: Vec<String>,
    usershadervalues: HashMap<String, String>,
}

/// Validated scene metadata without decoded textures.
#[derive(Debug, Clone)]
pub struct SceneSpec {
    pub profile: SceneProfile,
    pub texture_paths: [PathBuf; 3],
}

/// Load and validate one exact scene project selection.
///
/// `properties` are the project's declared color values. The scene file itself
/// must already be constrained to the project directory by the caller.
pub fn load(
    scene_path: &Path,
    project_root: &Path,
    properties: &HashMap<String, [f32; 3]>,
) -> Result<Scene> {
    let spec = parse(scene_path, project_root, properties)?;
    let mut textures = Vec::with_capacity(3);
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
    let [base, normal, cloth] = textures
        .try_into()
        .map_err(|_| anyhow::anyhow!("flag material must declare exactly three textures"))?;
    Ok(Scene {
        profile: spec.profile,
        base,
        normal,
        cloth,
    })
}

/// Validate one exact scene project selection without decoding textures.
///
/// Used to mark Steam projects as supported before the user selects one.
pub fn parse(
    scene_path: &Path,
    project_root: &Path,
    properties: &HashMap<String, [f32; 3]>,
) -> Result<SceneSpec> {
    let document: SceneDocument =
        read_json(scene_path, MAX_SCENE_BYTES).context("invalid scene.json")?;
    ensure!(
        document.objects.len() == 1,
        "unsupported scene with {} image objects; only one zero-rotation flag quad is supported",
        document.objects.len()
    );
    let object = &document.objects[0];
    let angles = parse_triplet(&object.angles).context("invalid object angles")?;
    ensure!(
        angles == [0.0, 0.0, 0.0],
        "unsupported object rotation {angles:?}; only a zero-rotation quad is supported"
    );
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
        "unsupported scene with {} material passes",
        material.passes.len()
    );
    let pass = &material.passes[0];
    ensure!(
        pass.shader == SUPPORTED_SHADER,
        "unsupported scene shader {:?}; only {SUPPORTED_SHADER:?} is supported",
        pass.shader
    );
    ensure!(
        pass.combos.get("TINT") == Some(&1),
        "unsupported flag combos; only TINT=1 is supported"
    );
    ensure!(
        pass.depthtest == "disabled" && pass.depthwrite == "disabled",
        "unsupported flag depth mode"
    );
    ensure!(
        pass.textures.len() == 3,
        "unsupported flag texture count {}",
        pass.textures.len()
    );
    let speed = *pass
        .constantshadervalues
        .get("Speed")
        .context("flag Speed is missing")?;
    let strength = *pass
        .constantshadervalues
        .get("Strength")
        .context("flag Strength is missing")?;
    ensure!(
        speed.is_finite() && strength.is_finite(),
        "flag animation constants must be finite"
    );
    ensure!(
        pass.usershadervalues.get("schemecolor").map(String::as_str) == Some("color1")
            && pass.usershadervalues.get("flagcolor1").map(String::as_str) == Some("color2")
            && pass.usershadervalues.get("flagcolor2").map(String::as_str) == Some("color3"),
        "unsupported flag color bindings"
    );
    let colors = FlagColors {
        background: *properties
            .get("schemecolor")
            .context("scene project does not declare schemecolor")?,
        accent1: *properties
            .get("flagcolor1")
            .context("scene project does not declare flagcolor1")?,
        accent2: *properties
            .get("flagcolor2")
            .context("scene project does not declare flagcolor2")?,
    };

    let material_root = material_path.parent().context("material has no parent")?;
    let mut texture_paths = Vec::with_capacity(3);
    for stem in &pass.textures {
        texture_paths.push(scene_relative(
            material_root,
            project_root,
            &format!("{stem}.tex"),
        )?);
    }
    let texture_paths: [PathBuf; 3] = texture_paths
        .try_into()
        .map_err(|_| anyhow::anyhow!("flag material must declare exactly three textures"))?;
    let profile = SceneProfile {
        scene_path: scene_path.to_path_buf(),
        projection,
        clear_color,
        camera,
        object: ObjectGeometry {
            id: object.id,
            name: object.name.clone(),
            center: [origin[0], origin[1]],
            extents: [
                (model.width as f32) * scale[0],
                (model.height as f32) * scale[1],
            ],
            model: [model.width, model.height],
        },
        colors,
        speed,
        strength,
        textures: [
            pass.textures[0].clone(),
            pass.textures[1].clone(),
            pass.textures[2].clone(),
        ],
        limitations: vec![
            "Renders one zero-rotation orthographic flag quad; camera eye/center metadata are retained, not used for placement.".into(),
            "Uses native TEX textures only; embedded PNG files are not substituted.".into(),
            "CPU reimplementation of the shipped flag vertex/fragment behavior; no engine code is executed.".into(),
        ],
    };
    Ok(SceneSpec {
        profile,
        texture_paths,
    })
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
        let object = &self.profile.object;
        Ok(SceneLayout {
            factor,
            left: (f64::from(width) - scene_width * factor) / 2.0,
            top: (f64::from(height) - scene_height * factor) / 2.0,
            left_edge: f64::from(object.center[0]) - f64::from(object.extents[0]) / 2.0,
            top_edge: f64::from(object.center[1]) - f64::from(object.extents[1]) / 2.0,
            extent_x: f64::from(object.extents[0]),
            extent_y: f64::from(object.extents[1]),
        })
    }

    fn output_uv(&self, layout: &SceneLayout, x: u32, y: u32) -> (f32, f32, bool) {
        let scene_x = (f64::from(x) + 0.5 - layout.left) / layout.factor;
        let scene_y = (f64::from(y) + 0.5 - layout.top) / layout.factor;
        let u = ((scene_x - layout.left_edge) / layout.extent_x) as f32;
        let v = ((scene_y - layout.top_edge) / layout.extent_y) as f32;
        (u, v, (0.0..=1.0).contains(&u) && (0.0..=1.0).contains(&v))
    }

    fn render_rows(
        &self,
        layout: &SceneLayout,
        width: u32,
        time: f32,
        start_y: u32,
        pixels: &mut [u8],
    ) {
        let speed = self.profile.speed;
        let strength = self.profile.strength;
        for (row_index, row) in pixels.chunks_exact_mut(width as usize * 4).enumerate() {
            let y = start_y + row_index as u32;
            for (x, pixel) in row.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                let (u, v, inside) = self.output_uv(layout, x as u32, y);
                let [b, g, r] = if inside {
                    self.flag_pixel(u, v, time, speed, strength)
                } else {
                    self.profile.clear_color
                };
                pixel.copy_from_slice(&[channel(b), channel(g), channel(r), 255]);
            }
        }
    }

    fn flag_pixel(&self, u: f32, v: f32, time: f32, speed: f32, strength: f32) -> [f32; 3] {
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
        let n1 = decompress_normal(self.normal.sample_repeat(n1x, n1y));
        let n2 = decompress_normal(self.normal.sample_repeat(n2x, n2y));
        let mut normal = [n1[0] * n2[0], n1[1] * n2[1], n1[2] * n2[2]];
        normal = mix3([0.0, 0.0, 1.0], normal, strength);
        normal = normalize(normal);

        let sample_u = u + normal[0] * 0.02;
        let sample_v = v + normal[1] * 0.02;
        let albedo = self.base.sample_clamp(sample_u, sample_v);
        let cloth = self.cloth.sample_repeat(sample_u * 4.0, sample_v * 4.0)[0];
        let colors = &self.profile.colors;
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
    ensure!(
        colors.contains_key("schemecolor")
            && colors.contains_key("flagcolor1")
            && colors.contains_key("flagcolor2"),
        "scene project must declare schemecolor, flagcolor1, and flagcolor2"
    );
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
        assert_eq!(scene.profile.object.extents, [6.0, 2.0]);
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
        assert!(
            mutate(&|document| document["objects"]
                .as_array_mut()
                .unwrap()
                .push(serde_json::json!({"angles":"0 0 0","id":8,"image":"models/quad.json","name":"copy","origin":"2 1 0","scale":"1 1 1"})))
            .contains("only one zero-rotation")
        );
        assert!(mutate(&|document| document["objects"][0]["angles"] =
            serde_json::Value::String("0 0 1".into()))
        .contains("rotation"));
        assert!(
            mutate(&|document| document["general"]["bloom"] = serde_json::Value::Bool(true))
                .contains("bloom")
        );
    }
}
