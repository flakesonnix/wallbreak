//! GPU rendering of scenes, images, and YUV video frames via glow.
//!
//! The shaders are direct ports of the CPU sampling in [`crate::scene`] and
//! share its layout and compositing rules, so a scene renders the same content
//! on either path. Callers own the GL context and the viewport; every draw
//! targets the currently bound framebuffer.

use std::{collections::HashMap, time::Instant};

use anyhow::{Context, Result, bail, ensure};
use glow::HasContext;
use image::RgbaImage;

use crate::{
    media::{Fit, Yuv420Frame, frame_len, yuv420_len},
    scene::{Blending, LayerGeometry, MaterialRuntime, Scene, SceneLayout, scene_layout},
    texture::Texture,
};

const VERTEX_SHADER: &str = r#"
in vec2 a_corner;
uniform vec2 u_origin;
uniform vec2 u_axis_u;
uniform vec2 u_axis_v;
out vec2 v_uv;
void main() {
    v_uv = a_corner;
    gl_Position = vec4(u_origin + a_corner.x * u_axis_u + a_corner.y * u_axis_v, 0.0, 1.0);
}
"#;

const IMAGE_FRAGMENT: &str = r#"
in vec2 v_uv;
out vec4 frag_color;
uniform sampler2D u_texture;
void main() {
    vec4 texel = texture(u_texture, v_uv);
    // Straight-alpha sources composite onto black, opaque, like the CPU fit.
    frag_color = vec4(texel.rgb * texel.a, 1.0);
}
"#;

const VIDEO_FRAGMENT: &str = r#"
in vec2 v_uv;
out vec4 frag_color;
uniform sampler2D u_y;
uniform sampler2D u_u;
uniform sampler2D u_v;
void main() {
    // FFmpeg emits limited-range BT.709 I420 for typical video sources.
    float y = (texture(u_y, v_uv).r * 255.0 - 16.0) / 219.0;
    float cb = (texture(u_u, v_uv).r * 255.0 - 128.0) / 224.0;
    float cr = (texture(u_v, v_uv).r * 255.0 - 128.0) / 224.0;
    vec3 rgb = vec3(
        y + 1.5748 * cr,
        y - 0.1873 * cb - 0.4681 * cr,
        y + 1.8556 * cb
    );
    frag_color = vec4(clamp(rgb, 0.0, 1.0), 1.0);
}
"#;

const GENERIC_FRAGMENT: &str = r#"
in vec2 v_uv;
out vec4 frag_color;
uniform sampler2D u_texture;
uniform float u_brightness;
uniform float u_alpha;
uniform float u_power;
uniform vec2 u_scroll;
uniform float u_time;
void main() {
    vec4 texel = texture(u_texture, v_uv + u_time * u_scroll);
    vec3 rgb = texel.rgb * u_brightness;
    if (u_power == 0.0) {
        rgb = vec3(1.0);
    } else if (u_power != 1.0) {
        if (u_power == floor(u_power)) {
            // GLSL pow() is undefined for negative bases, including integer powers.
            vec3 magnitude = pow(abs(rgb), vec3(u_power));
            rgb = mod(u_power, 2.0) == 0.0 ? magnitude : magnitude * sign(rgb);
        } else {
            rgb = pow(rgb, vec3(u_power));
        }
    }
    frag_color = vec4(rgb, texel.a * u_alpha);
}
"#;

const FLAG_FRAGMENT: &str = r#"
in vec2 v_uv;
out vec4 frag_color;
uniform sampler2D u_base;
uniform sampler2D u_normal;
uniform sampler2D u_cloth;
uniform vec3 u_background;
uniform vec3 u_accent1;
uniform vec3 u_accent2;
uniform float u_speed;
uniform float u_strength;
uniform float u_time;

vec3 decompress_normal(vec4 texel) {
    // DXT5 branch of the shipped DecompressNormal helper.
    float x = texel.g * 2.0 - 0.965;
    float y = texel.a * 2.0 - 1.0;
    float z = sqrt(max(0.0, 1.0 - x * x - y * y));
    return vec3(x, y, z);
}

void main() {
    float u = v_uv.x;
    float v = v_uv.y;
    // Direct transcription of flag.vert's scrolling normal coordinates and
    // flag.frag's tint, cloth, and lighting terms.
    float wave1 = u * 0.7 - u_time * u_speed;
    float wave2 = u * 0.3 - u_time * u_speed * 0.5;
    float v01 = v - 0.1;
    float n1x = wave1 - ((0.5 - u) * (1.0 - v)) * 3.0;
    n1x += 2.0 * (v01 * v01 * v01) * u * u;
    float n1y = v * 0.21;
    float n2x = wave2 - ((1.0 - u) * (1.0 - v)) * 2.0;
    float n2y = v * 0.21;
    vec3 n1 = decompress_normal(texture(u_normal, vec2(n1x, n1y)));
    vec3 n2 = decompress_normal(texture(u_normal, vec2(n2x, n2y)));
    vec3 normal = vec3(n1.x * n2.x, n1.y * n2.y, n1.z * n2.z);
    normal = mix(vec3(0.0, 0.0, 1.0), normal, u_strength);
    float length_squared = dot(normal, normal);
    normal = length_squared > 0.0 ? normal * inversesqrt(length_squared) : vec3(0.0, 0.0, 1.0);

    float sample_u = u + normal.x * 0.02;
    float sample_v = v + normal.y * 0.02;
    vec4 albedo = texture(u_base, vec2(sample_u, sample_v));
    float cloth = texture(u_cloth, vec2(sample_u * 4.0, sample_v * 4.0)).r;
    vec3 color = mix(u_background, u_accent1, albedo.r);
    color = mix(color, u_accent2, albedo.g);
    float cloth_light = albedo.b * cloth;
    color = color * cloth_light + cloth * 0.1;
    float light = 0.7 + (normal.x + normal.y) * 0.35355339;
    light += light * light * light * light * light * 0.5;
    float cloth_boost = light * max(0.0, cloth * 2.0 - 1.0);
    frag_color = vec4(color * (light + cloth_boost), 1.0);
}
"#;

const FLOW_FRAGMENT: &str = r#"
in vec2 v_uv;
out vec4 frag_color;
uniform sampler2D u_base;
uniform sampler2D u_mask;
uniform float u_brightness;
uniform float u_speed;
uniform float u_amount;
uniform vec2 u_bias;
uniform float u_time;

void main() {
    vec2 flow = (texture(u_mask, v_uv).rg - u_bias) * 2.0 * u_amount * 0.1;
    float c1 = fract(u_time * u_speed);
    float c2 = fract(u_time * u_speed + 0.5);
    float blend = 2.0 * abs(c1 - 0.5);
    vec4 a = texture(u_base, v_uv + flow * c1);
    vec4 b = texture(u_base, v_uv + flow * c2);
    vec4 rgb = mix(a, b, blend);
    frag_color = vec4(rgb.rgb * u_brightness, rgb.a);
}
"#;

const FLOW_LAYERS_FRAGMENT: &str = r#"
in vec2 v_uv;
out vec4 frag_color;
uniform sampler2D u_mask;
uniform sampler2D u_layer0;
uniform sampler2D u_layer1;
uniform sampler2D u_layer2;
uniform float u_brightness;
uniform float u_amount;
uniform vec3 u_speeds;
uniform vec2 u_bias;
uniform float u_time;

// blendLayers: rgb mixes by texel alpha, alpha takes the max.
vec4 blend_layer(vec4 albedo, sampler2D layer, vec2 flow, float speed, float offset) {
    float c1 = fract(u_time * speed + offset);
    float c2 = fract(u_time * speed + 0.5 + offset);
    float blend = 2.0 * abs(c1 - 0.5);
    vec4 a = texture(layer, v_uv + flow * c1);
    vec4 b = texture(layer, v_uv + flow * c2);
    vec4 texel = mix(a, b, blend);
    albedo.rgb = albedo.rgb + (texel.rgb - albedo.rgb) * texel.a;
    albedo.a = max(albedo.a, texel.a);
    return albedo;
}

void main() {
    vec2 flow = (texture(u_mask, v_uv).rg - u_bias) * 2.0 * u_amount * 0.1;
    vec4 albedo = vec4(0.0);
    albedo = blend_layer(albedo, u_layer0, flow, u_speeds.x, 0.0);
    albedo = blend_layer(albedo, u_layer1, flow, u_speeds.y, 0.0);
    albedo = blend_layer(albedo, u_layer2, flow, u_speeds.z, 0.3333);
    frag_color = vec4(albedo.rgb * u_brightness, albedo.a);
}
"#;

const SWAY_FRAGMENT: &str = r#"
in vec2 v_uv;
out vec4 frag_color;
uniform sampler2D u_base;
uniform sampler2D u_mask;
uniform float u_brightness;
uniform float u_speed;
uniform float u_amount;
uniform float u_time;
void main() {
    vec4 sway = texture(u_mask, v_uv);
    float phase = v_uv.x * 10.0 + sway.b;
    float t = u_time * u_speed;
    float amt = sin(t * 30.0 + phase)
        + sin(t * 27.0 + phase)
        + sin(t * 21.0 + phase)
        + sin(t * 7.0 + phase);
    vec2 offset = sway.rg * amt * u_amount * 0.01;
    vec4 albedo = texture(u_base, v_uv + offset);
    frag_color = vec4(albedo.rgb * u_brightness, albedo.a);
}
"#;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Wrap {
    Clamp,
    Repeat,
}

struct Program {
    program: glow::Program,
    uniforms: HashMap<String, glow::UniformLocation>,
}

impl Program {
    fn new(gl: &glow::Context, fragment_body: &str, uniform_names: &[&str]) -> Result<Self> {
        let vertex_source = shader_source(gl, VERTEX_SHADER)?;
        let fragment_source = shader_source(gl, fragment_body)?;
        let vertex = compile(gl, glow::VERTEX_SHADER, &vertex_source)
            .context("vertex shader compilation failed")?;
        let fragment = match compile(gl, glow::FRAGMENT_SHADER, &fragment_source) {
            Ok(fragment) => fragment,
            Err(error) => {
                unsafe { gl.delete_shader(vertex) };
                return Err(error.context("fragment shader compilation failed"));
            }
        };
        let program = match unsafe { gl.create_program() } {
            Ok(program) => program,
            Err(error) => {
                unsafe {
                    gl.delete_shader(vertex);
                    gl.delete_shader(fragment);
                }
                return Err(anyhow::Error::msg(error));
            }
        };
        let linked = (|| -> Result<()> {
            unsafe {
                gl.attach_shader(program, vertex);
                gl.attach_shader(program, fragment);
                gl.bind_attrib_location(program, 0, "a_corner");
                gl.link_program(program);
                if !gl.get_program_link_status(program) {
                    bail!("program link failed: {}", gl.get_program_info_log(program));
                }
            }
            Ok(())
        })();
        unsafe {
            gl.detach_shader(program, vertex);
            gl.detach_shader(program, fragment);
            gl.delete_shader(vertex);
            gl.delete_shader(fragment);
        }
        if let Err(error) = linked {
            unsafe { gl.delete_program(program) };
            return Err(error);
        }
        let mut uniforms = HashMap::with_capacity(uniform_names.len());
        for name in uniform_names {
            let location = unsafe { gl.get_uniform_location(program, name) };
            let Some(location) = location else {
                unsafe { gl.delete_program(program) };
                bail!("shader compiler optimized away uniform {name:?}");
            };
            uniforms.insert((*name).to_owned(), location);
        }
        Ok(Self { program, uniforms })
    }

    fn bind(&self, gl: &glow::Context) {
        unsafe { gl.use_program(Some(self.program)) };
    }

    fn set_f32(&self, gl: &glow::Context, name: &str, value: f32) {
        if let Some(location) = self.uniforms.get(name) {
            unsafe { gl.uniform_1_f32(Some(location), value) };
        }
    }

    fn set_2f(&self, gl: &glow::Context, name: &str, x: f32, y: f32) {
        if let Some(location) = self.uniforms.get(name) {
            unsafe { gl.uniform_2_f32_slice(Some(location), &[x, y]) };
        }
    }

    fn set_3f(&self, gl: &glow::Context, name: &str, value: [f32; 3]) {
        if let Some(location) = self.uniforms.get(name) {
            unsafe { gl.uniform_3_f32_slice(Some(location), &value) };
        }
    }

    fn set_texture(&self, gl: &glow::Context, name: &str, unit: u32) {
        if let Some(location) = self.uniforms.get(name) {
            unsafe { gl.uniform_1_i32(Some(location), unit as i32) };
        }
    }
}

fn shader_source(gl: &glow::Context, body: &str) -> Result<String> {
    let version = gl.version();
    if version.is_embedded {
        ensure!(
            version.major >= 3,
            "GPU rendering requires OpenGL ES 3.0 or newer"
        );
        Ok(format!(
            "#version 300 es\nprecision highp float;\nprecision highp int;\nprecision highp sampler2D;\n{body}"
        ))
    } else {
        ensure!(
            version.major > 3 || (version.major == 3 && version.minor >= 1),
            "GPU rendering requires OpenGL 3.1 or newer"
        );
        Ok(format!("#version 140\n{body}"))
    }
}

fn compile(gl: &glow::Context, kind: u32, source: &str) -> Result<glow::Shader> {
    let shader = unsafe { gl.create_shader(kind) }.map_err(anyhow::Error::msg)?;
    unsafe {
        gl.shader_source(shader, source);
        gl.compile_shader(shader);
    }
    if unsafe { gl.get_shader_compile_status(shader) } {
        Ok(shader)
    } else {
        let log = unsafe { gl.get_shader_info_log(shader) };
        unsafe { gl.delete_shader(shader) };
        bail!("{log}")
    }
}

struct Quad {
    vao: glow::VertexArray,
    vertex: glow::Buffer,
    index: glow::Buffer,
}

/// Affine corner placement in normalised device coordinates.
///
/// `origin` is the NDC position of the (u, v) = (0, 0) corner; `axis_u` and
/// `axis_v` are the NDC deltas towards (1, 0) and (0, 1).
#[derive(Debug, Clone, Copy, PartialEq)]
struct Placement {
    origin: [f32; 2],
    axis_u: [f32; 2],
    axis_v: [f32; 2],
}

impl Quad {
    fn new(gl: &glow::Context) -> Result<Self> {
        let vertices: [f32; 8] = [0.0, 0.0, 1.0, 0.0, 1.0, 1.0, 0.0, 1.0];
        let indices: [u16; 6] = [0, 1, 2, 0, 2, 3];
        unsafe {
            let vao = gl.create_vertex_array().map_err(anyhow::Error::msg)?;
            let vertex = match gl.create_buffer() {
                Ok(vertex) => vertex,
                Err(error) => {
                    gl.delete_vertex_array(vao);
                    return Err(anyhow::Error::msg(error));
                }
            };
            let index = match gl.create_buffer() {
                Ok(index) => index,
                Err(error) => {
                    gl.delete_vertex_array(vao);
                    gl.delete_buffer(vertex);
                    return Err(anyhow::Error::msg(error));
                }
            };
            gl.bind_vertex_array(Some(vao));
            gl.bind_buffer(glow::ARRAY_BUFFER, Some(vertex));
            gl.buffer_data_u8_slice(glow::ARRAY_BUFFER, bytes_of(&vertices), glow::STATIC_DRAW);
            gl.enable_vertex_attrib_array(0);
            gl.vertex_attrib_pointer_f32(0, 2, glow::FLOAT, false, 8, 0);
            gl.bind_buffer(glow::ELEMENT_ARRAY_BUFFER, Some(index));
            gl.buffer_data_u8_slice(
                glow::ELEMENT_ARRAY_BUFFER,
                bytes_of(&indices),
                glow::STATIC_DRAW,
            );
            gl.bind_vertex_array(None);
            gl.bind_buffer(glow::ARRAY_BUFFER, None);
            let quad = Self { vao, vertex, index };
            if let Err(error) = check_gl(gl) {
                quad.destroy(gl);
                return Err(error.context("quad upload was rejected by the driver"));
            }
            Ok(quad)
        }
    }

    fn destroy(&self, gl: &glow::Context) {
        unsafe {
            gl.delete_vertex_array(self.vao);
            gl.delete_buffer(self.vertex);
            gl.delete_buffer(self.index);
        }
    }
}

fn bytes_of<T>(slice: &[T]) -> &[u8] {
    unsafe { std::slice::from_raw_parts(slice.as_ptr().cast::<u8>(), std::mem::size_of_val(slice)) }
}

/// Preserve the CPU compositor's f32 intermediates until the final byte conversion.
struct SceneTarget {
    framebuffer: glow::Framebuffer,
    texture: glow::Texture,
    width: u32,
    height: u32,
}

impl SceneTarget {
    fn new(gl: &glow::Context, width: u32, height: u32) -> Result<Self> {
        // RGBA32F storage is four times the admitted RGBA8 output size.
        frame_len(width, height).context("scene compositor output exceeds its frame budget")?;
        if gl.version().is_embedded {
            let extensions = gl.supported_extensions();
            ensure!(
                extensions.contains("GL_EXT_color_buffer_float")
                    && extensions.contains("GL_EXT_float_blend"),
                "GPU scene compositing requires GL_EXT_color_buffer_float and GL_EXT_float_blend on OpenGL ES"
            );
        }
        let max_size = unsafe { gl.get_parameter_i32(glow::MAX_TEXTURE_SIZE) };
        ensure!(
            max_size > 0 && width <= max_size as u32 && height <= max_size as u32,
            "scene compositor output exceeds GL_MAX_TEXTURE_SIZE ({max_size})"
        );
        let framebuffer = unsafe { gl.create_framebuffer() }.map_err(anyhow::Error::msg)?;
        let texture = match unsafe { gl.create_texture() } {
            Ok(texture) => texture,
            Err(error) => {
                unsafe { gl.delete_framebuffer(framebuffer) };
                return Err(anyhow::Error::msg(error));
            }
        };
        let target = Self {
            framebuffer,
            texture,
            width,
            height,
        };
        let previous = unsafe {
            std::num::NonZeroU32::new(gl.get_parameter_i32(glow::DRAW_FRAMEBUFFER_BINDING) as u32)
                .map(glow::NativeFramebuffer)
        };
        let result = (|| -> Result<()> {
            let _unpack = UnpackState::tight(gl, 0);
            let status;
            unsafe {
                gl.bind_texture(glow::TEXTURE_2D, Some(texture));
                for parameter in [glow::TEXTURE_MIN_FILTER, glow::TEXTURE_MAG_FILTER] {
                    gl.tex_parameter_i32(glow::TEXTURE_2D, parameter, glow::NEAREST as i32);
                }
                for parameter in [glow::TEXTURE_WRAP_S, glow::TEXTURE_WRAP_T] {
                    gl.tex_parameter_i32(glow::TEXTURE_2D, parameter, glow::CLAMP_TO_EDGE as i32);
                }
                gl.tex_image_2d(
                    glow::TEXTURE_2D,
                    0,
                    glow::RGBA32F as i32,
                    width as i32,
                    height as i32,
                    0,
                    glow::RGBA,
                    glow::FLOAT,
                    None,
                );
                gl.bind_framebuffer(glow::DRAW_FRAMEBUFFER, Some(framebuffer));
                gl.framebuffer_texture_2d(
                    glow::DRAW_FRAMEBUFFER,
                    glow::COLOR_ATTACHMENT0,
                    glow::TEXTURE_2D,
                    Some(texture),
                    0,
                );
                status = gl.check_framebuffer_status(glow::DRAW_FRAMEBUFFER);
            }
            check_gl(gl).context("floating-point scene allocation was rejected by the driver")?;
            ensure!(
                status == glow::FRAMEBUFFER_COMPLETE,
                "floating-point scene framebuffer is incomplete ({status:#x})"
            );
            Ok(())
        })();
        unsafe { gl.bind_framebuffer(glow::DRAW_FRAMEBUFFER, previous) };
        if let Err(error) = result {
            target.destroy(gl);
            return Err(error);
        }
        Ok(target)
    }

    fn destroy(&self, gl: &glow::Context) {
        unsafe {
            gl.delete_framebuffer(self.framebuffer);
            gl.delete_texture(self.texture);
        }
    }
}

/// GPU programs shared by every scene, image, and video draw.
pub(crate) struct Renderer {
    quad: Quad,
    image: Program,
    video: Program,
    flag: Program,
    generic: Program,
    flow: Program,
    flow_layers: Program,
    sway: Program,
    scene_target: Option<SceneTarget>,
}

impl Renderer {
    /// Compile every program. The GL context must be current on this thread.
    pub(crate) fn new(gl: &glow::Context) -> Result<Self> {
        let quad = Quad::new(gl).context("cannot create the shared quad geometry")?;
        let mut programs = Vec::with_capacity(7);
        let mut create = |fragment, uniforms: &[&str]| -> Result<Program> {
            let program = Program::new(gl, fragment, uniforms)?;
            programs.push(program.program);
            Ok(program)
        };
        let result = (|| -> Result<(Program, Program, Program, Program, Program, Program, Program)> {
            let image = create(IMAGE_FRAGMENT, &["u_texture", "u_origin", "u_axis_u", "u_axis_v"])?;
            let video = create(
                VIDEO_FRAGMENT,
                &["u_y", "u_u", "u_v", "u_origin", "u_axis_u", "u_axis_v"],
            )?;
            let flag = create(
                FLAG_FRAGMENT,
                &[
                    "u_base",
                    "u_normal",
                    "u_cloth",
                    "u_background",
                    "u_accent1",
                    "u_accent2",
                    "u_speed",
                    "u_strength",
                    "u_time",
                    "u_origin",
                    "u_axis_u",
                    "u_axis_v",
                ],
            )?;
            let generic = create(
                GENERIC_FRAGMENT,
                &[
                    "u_texture",
                    "u_brightness",
                    "u_alpha",
                    "u_power",
                    "u_scroll",
                    "u_time",
                    "u_origin",
                    "u_axis_u",
                    "u_axis_v",
                ],
            )?;
            let flow = create(
                FLOW_FRAGMENT,
                &[
                    "u_base",
                    "u_mask",
                    "u_brightness",
                    "u_speed",
                    "u_amount",
                    "u_bias",
                    "u_time",
                    "u_origin",
                    "u_axis_u",
                    "u_axis_v",
                ],
            )?;
            let flow_layers = create(
                FLOW_LAYERS_FRAGMENT,
                &[
                    "u_mask",
                    "u_layer0",
                    "u_layer1",
                    "u_layer2",
                    "u_brightness",
                    "u_amount",
                    "u_speeds",
                    "u_bias",
                    "u_time",
                    "u_origin",
                    "u_axis_u",
                    "u_axis_v",
                ],
            )?;
            let sway = create(
                SWAY_FRAGMENT,
                &[
                    "u_base",
                    "u_mask",
                    "u_brightness",
                    "u_speed",
                    "u_amount",
                    "u_time",
                    "u_origin",
                    "u_axis_u",
                    "u_axis_v",
                ],
            )?;
            Ok((image, video, flag, generic, flow, flow_layers, sway))
        })();
        match result {
            Ok((image, video, flag, generic, flow, flow_layers, sway)) => Ok(Self {
                quad,
                image,
                video,
                flag,
                generic,
                flow,
                flow_layers,
                sway,
                scene_target: None,
            }),
            Err(error) => {
                for program in programs {
                    unsafe { gl.delete_program(program) };
                }
                quad.destroy(gl);
                Err(error)
            }
        }
    }

    fn draw_quad(&self, gl: &glow::Context, program: &Program, placement: Placement) {
        program.set_2f(gl, "u_origin", placement.origin[0], placement.origin[1]);
        program.set_2f(gl, "u_axis_u", placement.axis_u[0], placement.axis_u[1]);
        program.set_2f(gl, "u_axis_v", placement.axis_v[0], placement.axis_v[1]);
        unsafe {
            gl.bind_vertex_array(Some(self.quad.vao));
            gl.draw_elements(glow::TRIANGLES, 6, glow::UNSIGNED_SHORT, 0);
            gl.bind_vertex_array(None);
        }
    }

    /// Render one scene frame into the bound framebuffer.
    ///
    /// `width` and `height` are the physical framebuffer pixels; the caller has
    /// already set the viewport to cover them.
    pub(crate) fn draw_scene(
        &mut self,
        gl: &glow::Context,
        scene: &GpuScene,
        time: f32,
        width: u32,
        height: u32,
        fit: Fit,
    ) -> Result<()> {
        ensure!(time.is_finite() && time >= 0.0, "scene time must be finite");
        frame_len(width, height)?;
        let layout = scene_layout(scene.projection, width, height, fit)?;
        let mut color_mask = [0; 4];
        let mut viewport = [0; 4];
        let framebuffer;
        let scissor;
        unsafe {
            gl.get_parameter_i32_slice(glow::COLOR_WRITEMASK, &mut color_mask);
            gl.get_parameter_i32_slice(glow::VIEWPORT, &mut viewport);
            framebuffer = std::num::NonZeroU32::new(
                gl.get_parameter_i32(glow::DRAW_FRAMEBUFFER_BINDING) as u32,
            )
            .map(glow::NativeFramebuffer);
            scissor = gl.is_enabled(glow::SCISSOR_TEST);
        }
        let mut result = (|| -> Result<()> {
            if self
                .scene_target
                .as_ref()
                .is_none_or(|target| (target.width, target.height) != (width, height))
            {
                // Release old storage before growing, rather than retaining two float frames.
                if let Some(target) = self.scene_target.take() {
                    target.destroy(gl);
                }
                self.scene_target = Some(SceneTarget::new(gl, width, height)?);
            }
            let target = self.scene_target.as_ref().unwrap();
            unsafe {
                gl.bind_framebuffer(glow::DRAW_FRAMEBUFFER, Some(target.framebuffer));
                gl.viewport(0, 0, width as i32, height as i32);
                gl.disable(glow::SCISSOR_TEST);
                gl.color_mask(true, true, true, true);
                gl.disable(glow::DEPTH_TEST);
                gl.disable(glow::CULL_FACE);
                gl.disable(glow::BLEND);
                gl.clear_buffer_f32_slice(
                    glow::COLOR,
                    0,
                    &[scene.clear[0], scene.clear[1], scene.clear[2], 1.0],
                );
                // RGB keeps unclamped f32 intermediates; the final scene remains opaque.
                gl.color_mask(true, true, true, false);
            }
            for layer in &scene.layers {
                let placement = layer_placement(&layout, &layer.geometry, width, height);
                self.draw_layer(gl, &layer.material, placement, time, layer.blending);
            }
            Ok(())
        })();
        unsafe {
            gl.bind_framebuffer(glow::DRAW_FRAMEBUFFER, framebuffer);
            gl.viewport(viewport[0], viewport[1], viewport[2], viewport[3]);
            if scissor {
                gl.enable(glow::SCISSOR_TEST);
            } else {
                gl.disable(glow::SCISSOR_TEST);
            }
        }
        if result.is_ok() {
            unsafe {
                gl.disable(glow::BLEND);
                gl.color_mask(true, true, true, true);
            }
            self.image.bind(gl);
            bind_texture(gl, 0, self.scene_target.as_ref().unwrap().texture);
            self.image.set_texture(gl, "u_texture", 0);
            // FBO textures are bottom-up, unlike decoded source images.
            self.draw_quad(
                gl,
                &self.image,
                Placement {
                    origin: [-1.0, -1.0],
                    axis_u: [2.0, 0.0],
                    axis_v: [0.0, 2.0],
                },
            );
            result = check_gl(gl).context("scene compositing failed");
        }
        unsafe {
            gl.color_mask(
                color_mask[0] != 0,
                color_mask[1] != 0,
                color_mask[2] != 0,
                color_mask[3] != 0,
            );
        }
        result
    }

    fn draw_layer(
        &self,
        gl: &glow::Context,
        material: &GpuMaterial,
        placement: Placement,
        time: f32,
        blending: Blending,
    ) {
        binding(gl, blending);
        match material {
            GpuMaterial::Flag {
                base,
                normal,
                cloth,
                background,
                accent1,
                accent2,
                speed,
                strength,
            } => {
                let program = &self.flag;
                program.bind(gl);
                bind_texture(gl, 0, *base);
                bind_texture(gl, 1, *normal);
                bind_texture(gl, 2, *cloth);
                program.set_texture(gl, "u_base", 0);
                program.set_texture(gl, "u_normal", 1);
                program.set_texture(gl, "u_cloth", 2);
                program.set_3f(gl, "u_background", *background);
                program.set_3f(gl, "u_accent1", *accent1);
                program.set_3f(gl, "u_accent2", *accent2);
                program.set_f32(gl, "u_speed", *speed);
                program.set_f32(gl, "u_strength", *strength);
                program.set_f32(gl, "u_time", time);
                self.draw_quad(gl, program, placement);
            }
            GpuMaterial::Generic {
                texture,
                brightness,
                alpha,
                power,
                scroll,
            } => {
                let program = &self.generic;
                program.bind(gl);
                bind_texture(gl, 0, *texture);
                program.set_texture(gl, "u_texture", 0);
                program.set_f32(gl, "u_brightness", *brightness);
                program.set_f32(gl, "u_alpha", *alpha);
                program.set_f32(gl, "u_power", *power);
                program.set_2f(gl, "u_scroll", scroll[0], scroll[1]);
                program.set_f32(gl, "u_time", time);
                self.draw_quad(gl, program, placement);
            }
            GpuMaterial::Flow {
                base,
                mask,
                brightness,
                speed,
                amount,
                bias,
            } => {
                let program = &self.flow;
                program.bind(gl);
                bind_texture(gl, 0, *base);
                bind_texture(gl, 1, *mask);
                program.set_texture(gl, "u_base", 0);
                program.set_texture(gl, "u_mask", 1);
                program.set_f32(gl, "u_brightness", *brightness);
                program.set_f32(gl, "u_speed", *speed);
                program.set_f32(gl, "u_amount", *amount);
                program.set_2f(gl, "u_bias", bias[0], bias[1]);
                program.set_f32(gl, "u_time", time);
                self.draw_quad(gl, program, placement);
            }
            GpuMaterial::FlowLayers {
                mask,
                layers,
                brightness,
                amount,
                speeds,
                bias,
            } => {
                let program = &self.flow_layers;
                program.bind(gl);
                bind_texture(gl, 0, *mask);
                bind_texture(gl, 1, layers[0]);
                bind_texture(gl, 2, layers[1]);
                bind_texture(gl, 3, layers[2]);
                program.set_texture(gl, "u_mask", 0);
                program.set_texture(gl, "u_layer0", 1);
                program.set_texture(gl, "u_layer1", 2);
                program.set_texture(gl, "u_layer2", 3);
                program.set_f32(gl, "u_brightness", *brightness);
                program.set_f32(gl, "u_amount", *amount);
                program.set_3f(gl, "u_speeds", *speeds);
                program.set_2f(gl, "u_bias", bias[0], bias[1]);
                program.set_f32(gl, "u_time", time);
                self.draw_quad(gl, program, placement);
            }
            GpuMaterial::Sway {
                base,
                mask,
                brightness,
                speed,
                amount,
            } => {
                let program = &self.sway;
                program.bind(gl);
                bind_texture(gl, 0, *base);
                bind_texture(gl, 1, *mask);
                program.set_texture(gl, "u_base", 0);
                program.set_texture(gl, "u_mask", 1);
                program.set_f32(gl, "u_brightness", *brightness);
                program.set_f32(gl, "u_speed", *speed);
                program.set_f32(gl, "u_amount", *amount);
                program.set_f32(gl, "u_time", time);
                self.draw_quad(gl, program, placement);
            }
        }
    }

    /// Draw a decoded image over the whole bound framebuffer.
    pub(crate) fn draw_image(
        &self,
        gl: &glow::Context,
        image: &GpuImage,
        width: u32,
        height: u32,
        fit: Fit,
    ) -> Result<()> {
        ensure!(width > 0 && height > 0, "image viewport must be nonzero");
        let placement = image_placement((image.width, image.height), width, height, fit)?;
        let mut color_mask = [0; 4];
        unsafe {
            gl.get_parameter_i32_slice(glow::COLOR_WRITEMASK, &mut color_mask);
            gl.color_mask(true, true, true, true);
            gl.disable(glow::DEPTH_TEST);
            gl.disable(glow::CULL_FACE);
            gl.disable(glow::BLEND);
            gl.clear_color(0.0, 0.0, 0.0, 1.0);
            gl.clear(glow::COLOR_BUFFER_BIT);
        }
        let program = &self.image;
        program.bind(gl);
        bind_texture(gl, 0, image.texture);
        program.set_texture(gl, "u_texture", 0);
        self.draw_quad(gl, program, placement);
        unsafe {
            gl.color_mask(
                color_mask[0] != 0,
                color_mask[1] != 0,
                color_mask[2] != 0,
                color_mask[3] != 0,
            );
        }
        Ok(())
    }

    /// Draw the latest uploaded video frame over the whole bound framebuffer.
    ///
    /// Frames arrive already fitted to the output size, so no placement is
    /// computed here.
    pub(crate) fn draw_video(&self, gl: &glow::Context, video: &GpuVideo) -> Result<()> {
        ensure!(
            video.uploaded,
            "the video surface has no complete frame yet"
        );
        unsafe {
            gl.disable(glow::DEPTH_TEST);
            gl.disable(glow::CULL_FACE);
            gl.disable(glow::BLEND);
        }
        let program = &self.video;
        program.bind(gl);
        bind_texture(gl, 0, video.y);
        bind_texture(gl, 1, video.u);
        bind_texture(gl, 2, video.v);
        program.set_texture(gl, "u_y", 0);
        program.set_texture(gl, "u_u", 1);
        program.set_texture(gl, "u_v", 2);
        self.draw_quad(
            gl,
            program,
            Placement {
                origin: [-1.0, 1.0],
                axis_u: [2.0, 0.0],
                axis_v: [0.0, -2.0],
            },
        );
        Ok(())
    }

    /// Delete every GL object owned by this renderer.
    pub(crate) fn destroy(&self, gl: &glow::Context) {
        if let Some(target) = &self.scene_target {
            target.destroy(gl);
        }
        self.quad.destroy(gl);
        for program in [
            &self.image,
            &self.video,
            &self.flag,
            &self.generic,
            &self.flow,
            &self.flow_layers,
            &self.sway,
        ] {
            unsafe { gl.delete_program(program.program) };
        }
    }
}

fn binding(gl: &glow::Context, blending: Blending) {
    unsafe {
        match blending {
            Blending::Opaque => gl.disable(glow::BLEND),
            Blending::Translucent => {
                gl.enable(glow::BLEND);
                gl.blend_equation_separate(glow::FUNC_ADD, glow::FUNC_ADD);
                gl.blend_func_separate(
                    glow::SRC_ALPHA,
                    glow::ONE_MINUS_SRC_ALPHA,
                    glow::ONE,
                    glow::ONE_MINUS_SRC_ALPHA,
                );
            }
            Blending::Additive => {
                gl.enable(glow::BLEND);
                gl.blend_equation_separate(glow::FUNC_ADD, glow::FUNC_ADD);
                gl.blend_func_separate(glow::SRC_ALPHA, glow::ONE, glow::ONE, glow::ONE);
            }
        }
    }
}

fn bind_texture(gl: &glow::Context, unit: u32, texture: glow::Texture) {
    unsafe {
        gl.active_texture(glow::TEXTURE0 + unit);
        gl.bind_texture(glow::TEXTURE_2D, Some(texture));
    }
}

fn ndc(x: f64, y: f64, width: u32, height: u32) -> [f32; 2] {
    [
        (2.0 * x / f64::from(width) - 1.0) as f32,
        (1.0 - 2.0 * y / f64::from(height)) as f32,
    ]
}

/// Map scene-space layer corners through the fit layout into NDC.
///
/// This is the exact inverse of the CPU `Scene::layer_uv` mapping, so both
/// paths agree on which scene point each output pixel shows.
fn layer_placement(
    layout: &SceneLayout,
    geometry: &LayerGeometry,
    width: u32,
    height: u32,
) -> Placement {
    let (sin, cos) = f64::from(geometry.rotation).sin_cos();
    let (extent_x, extent_y) = (
        f64::from(geometry.extents[0]),
        f64::from(geometry.extents[1]),
    );
    let map = |u: f64, v: f64| -> (f64, f64) {
        let du = u - 0.5;
        let dv = v - 0.5;
        let scene_x = f64::from(geometry.center[0]) + du * extent_x * cos - dv * extent_y * sin;
        let scene_y = f64::from(geometry.center[1]) + du * extent_x * sin + dv * extent_y * cos;
        (
            scene_x * layout.factor + layout.left,
            scene_y * layout.factor + layout.top,
        )
    };
    let origin = map(0.0, 0.0);
    let along_u = map(1.0, 0.0);
    let along_v = map(0.0, 1.0);
    let origin = ndc(origin.0, origin.1, width, height);
    let along_u = ndc(along_u.0, along_u.1, width, height);
    let along_v = ndc(along_v.0, along_v.1, width, height);
    Placement {
        origin,
        axis_u: [along_u[0] - origin[0], along_u[1] - origin[1]],
        axis_v: [along_v[0] - origin[0], along_v[1] - origin[1]],
    }
}

fn image_placement(source: (u32, u32), width: u32, height: u32, fit: Fit) -> Result<Placement> {
    let (source_width, source_height) = (f64::from(source.0), f64::from(source.1));
    ensure!(
        source_width > 0.0 && source_height > 0.0,
        "image source has zero dimensions"
    );
    let factor = match fit {
        Fit::Cover => (f64::from(width) / source_width).max(f64::from(height) / source_height),
        Fit::Contain => (f64::from(width) / source_width).min(f64::from(height) / source_height),
    };
    let drawn_width = source_width * factor;
    let drawn_height = source_height * factor;
    let left = (f64::from(width) - drawn_width) / 2.0;
    let top = (f64::from(height) - drawn_height) / 2.0;
    let origin = ndc(left, top, width, height);
    let along_u = ndc(left + drawn_width, top, width, height);
    let along_v = ndc(left, top + drawn_height, width, height);
    Ok(Placement {
        origin,
        axis_u: [along_u[0] - origin[0], along_u[1] - origin[1]],
        axis_v: [along_v[0] - origin[0], along_v[1] - origin[1]],
    })
}

#[derive(Debug, Clone)]
enum GpuMaterial {
    Flag {
        base: glow::Texture,
        normal: glow::Texture,
        cloth: glow::Texture,
        background: [f32; 3],
        accent1: [f32; 3],
        accent2: [f32; 3],
        speed: f32,
        strength: f32,
    },
    Generic {
        texture: glow::Texture,
        brightness: f32,
        alpha: f32,
        power: f32,
        scroll: [f32; 2],
    },
    Flow {
        base: glow::Texture,
        mask: glow::Texture,
        brightness: f32,
        speed: f32,
        amount: f32,
        bias: [f32; 2],
    },
    FlowLayers {
        mask: glow::Texture,
        layers: [glow::Texture; 3],
        brightness: f32,
        amount: f32,
        speeds: [f32; 3],
        bias: [f32; 2],
    },
    Sway {
        base: glow::Texture,
        mask: glow::Texture,
        brightness: f32,
        speed: f32,
        amount: f32,
    },
}

impl GpuMaterial {
    fn textures(&self) -> Vec<glow::Texture> {
        match self {
            GpuMaterial::Flag {
                base,
                normal,
                cloth,
                ..
            } => vec![*base, *normal, *cloth],
            GpuMaterial::Generic { texture, .. } => vec![*texture],
            GpuMaterial::Flow { base, mask, .. } | GpuMaterial::Sway { base, mask, .. } => {
                vec![*base, *mask]
            }
            GpuMaterial::FlowLayers { mask, layers, .. } => {
                vec![*mask, layers[0], layers[1], layers[2]]
            }
        }
    }
}

struct GpuLayer {
    geometry: LayerGeometry,
    blending: Blending,
    material: GpuMaterial,
}

/// Scene textures and material parameters uploaded for GPU rendering.
pub(crate) struct GpuScene {
    clear: [f32; 3],
    projection: [u32; 2],
    layers: Vec<GpuLayer>,
}

impl GpuScene {
    /// Upload every scene texture. The GL context must be current on this thread.
    pub(crate) fn new(gl: &glow::Context, scene: &Scene) -> Result<Self> {
        let profile = scene.profile();
        let mut uploaded: Vec<glow::Texture> = Vec::new();
        let result = (|| -> Result<Vec<GpuLayer>> {
            let mut layers = Vec::with_capacity(scene.runtime_layers().len());
            for layer in scene.runtime_layers() {
                let material = match &layer.material {
                    MaterialRuntime::Flag {
                        colors,
                        speed,
                        strength,
                        base,
                        normal,
                        cloth,
                    } => {
                        let base_texture =
                            upload_scene_texture(gl, base, Wrap::Clamp).context("flag base")?;
                        uploaded.push(base_texture);
                        let normal_texture = upload_scene_texture(gl, normal, Wrap::Repeat)
                            .context("flag normal")?;
                        uploaded.push(normal_texture);
                        let cloth_texture =
                            upload_scene_texture(gl, cloth, Wrap::Repeat).context("flag cloth")?;
                        uploaded.push(cloth_texture);
                        GpuMaterial::Flag {
                            base: base_texture,
                            normal: normal_texture,
                            cloth: cloth_texture,
                            background: colors.background,
                            accent1: colors.accent1,
                            accent2: colors.accent2,
                            speed: *speed,
                            strength: *strength,
                        }
                    }
                    MaterialRuntime::Generic {
                        brightness,
                        alpha,
                        power,
                        scroll,
                        tex,
                        ..
                    } => {
                        let texture =
                            upload_scene_texture(gl, tex, Wrap::Repeat).context("generic")?;
                        uploaded.push(texture);
                        GpuMaterial::Generic {
                            texture,
                            brightness: *brightness,
                            alpha: *alpha,
                            power: *power,
                            scroll: crate::scene::runtime_scroll(*scroll),
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
                        let base_texture =
                            upload_scene_texture(gl, base, Wrap::Repeat).context("flow base")?;
                        uploaded.push(base_texture);
                        let mask_texture =
                            upload_scene_texture(gl, mask, Wrap::Repeat).context("flow mask")?;
                        uploaded.push(mask_texture);
                        GpuMaterial::Flow {
                            base: base_texture,
                            mask: mask_texture,
                            brightness: *brightness,
                            speed: *speed,
                            amount: *amount,
                            bias: *bias,
                        }
                    }
                    MaterialRuntime::FlowLayers {
                        brightness,
                        amount,
                        speeds,
                        bias,
                        mask,
                        layers,
                    } => {
                        let mask_texture =
                            upload_scene_texture(gl, mask, Wrap::Repeat).context("flow mask")?;
                        uploaded.push(mask_texture);
                        let mut layer_textures = [mask_texture; 3];
                        for (index, layer) in layers.iter().enumerate() {
                            let texture = upload_scene_texture(gl, layer, Wrap::Repeat)
                                .context("flow layer")?;
                            uploaded.push(texture);
                            layer_textures[index] = texture;
                        }
                        GpuMaterial::FlowLayers {
                            mask: mask_texture,
                            layers: layer_textures,
                            brightness: *brightness,
                            amount: *amount,
                            speeds: *speeds,
                            bias: *bias,
                        }
                    }
                    MaterialRuntime::Sway {
                        brightness,
                        speed,
                        amount,
                        base,
                        mask,
                    } => {
                        let base_texture =
                            upload_scene_texture(gl, base, Wrap::Repeat).context("sway base")?;
                        uploaded.push(base_texture);
                        let mask_texture =
                            upload_scene_texture(gl, mask, Wrap::Repeat).context("sway mask")?;
                        uploaded.push(mask_texture);
                        GpuMaterial::Sway {
                            base: base_texture,
                            mask: mask_texture,
                            brightness: *brightness,
                            speed: *speed,
                            amount: *amount,
                        }
                    }
                };
                layers.push(GpuLayer {
                    geometry: layer.geometry.clone(),
                    blending: layer.material.blending(),
                    material,
                });
            }
            Ok(layers)
        })();
        match result {
            Ok(layers) => Ok(Self {
                clear: profile.clear_color,
                projection: profile.projection,
                layers,
            }),
            Err(error) => {
                for texture in uploaded {
                    unsafe { gl.delete_texture(texture) };
                }
                Err(error)
            }
        }
    }

    pub(crate) fn destroy(&self, gl: &glow::Context) {
        for layer in &self.layers {
            for texture in layer.material.textures() {
                unsafe { gl.delete_texture(texture) };
            }
        }
    }
}

/// One uploaded still image with its source dimensions.
pub(crate) struct GpuImage {
    texture: glow::Texture,
    width: u32,
    height: u32,
}

impl GpuImage {
    /// Upload one decoded image with mipmaps for high-quality downscaling.
    pub(crate) fn new(gl: &glow::Context, image: &RgbaImage) -> Result<Self> {
        let (width, height) = image.dimensions();
        let texture = upload_rgba(gl, width, height, None, image.as_raw(), Wrap::Clamp, true)
            .context("cannot upload the image")?;
        Ok(Self {
            texture,
            width,
            height,
        })
    }

    pub(crate) fn destroy(&self, gl: &glow::Context) {
        unsafe { gl.delete_texture(self.texture) };
    }
}

/// The three I420 plane textures of one video stream.
pub(crate) struct GpuVideo {
    y: glow::Texture,
    u: glow::Texture,
    v: glow::Texture,
    width: u32,
    height: u32,
    uploaded: bool,
}

impl GpuVideo {
    pub(crate) fn new(gl: &glow::Context) -> Result<Self> {
        let create = || -> Result<glow::Texture> {
            let texture = unsafe { gl.create_texture() }.map_err(anyhow::Error::msg)?;
            unsafe {
                gl.bind_texture(glow::TEXTURE_2D, Some(texture));
                gl.tex_parameter_i32(
                    glow::TEXTURE_2D,
                    glow::TEXTURE_MIN_FILTER,
                    glow::LINEAR as i32,
                );
                gl.tex_parameter_i32(
                    glow::TEXTURE_2D,
                    glow::TEXTURE_MAG_FILTER,
                    glow::LINEAR as i32,
                );
                gl.tex_parameter_i32(
                    glow::TEXTURE_2D,
                    glow::TEXTURE_WRAP_S,
                    glow::CLAMP_TO_EDGE as i32,
                );
                gl.tex_parameter_i32(
                    glow::TEXTURE_2D,
                    glow::TEXTURE_WRAP_T,
                    glow::CLAMP_TO_EDGE as i32,
                );
            }
            Ok(texture)
        };
        let y = create().context("cannot create the video luma plane")?;
        let u = match create() {
            Ok(u) => u,
            Err(error) => {
                unsafe { gl.delete_texture(y) };
                return Err(error);
            }
        };
        let v = match create() {
            Ok(v) => v,
            Err(error) => {
                unsafe {
                    gl.delete_texture(y);
                    gl.delete_texture(u);
                }
                return Err(error);
            }
        };
        Ok(Self {
            y,
            u,
            v,
            width: 0,
            height: 0,
            uploaded: false,
        })
    }

    /// Upload one complete decoded frame, reallocating on size changes.
    pub(crate) fn upload(&mut self, gl: &glow::Context, frame: &Yuv420Frame) -> Result<()> {
        self.uploaded = false;
        let max_size = unsafe { gl.get_parameter_i32(glow::MAX_TEXTURE_SIZE) };
        ensure!(
            max_size > 0 && frame.width <= max_size as u32 && frame.height <= max_size as u32,
            "video frame exceeds GL_MAX_TEXTURE_SIZE ({max_size})"
        );
        let expected = yuv420_len(frame.width, frame.height)?;
        let chroma_width = frame.width.div_ceil(2);
        let chroma_height = frame.height.div_ceil(2);
        // yuv420_len has already checked the dimensions and bounded these products.
        let luma = (frame.width as usize) * (frame.height as usize);
        let chroma = (expected - luma) / 2;
        ensure!(
            frame.y.len() == luma && frame.u.len() == chroma && frame.v.len() == chroma,
            "I420 plane sizes do not match {}x{}",
            frame.width,
            frame.height
        );
        let reallocate = (frame.width, frame.height) != (self.width, self.height);
        // A failed allocation or plane upload must force complete reallocation on retry.
        self.width = 0;
        self.height = 0;
        let unpack = UnpackState::tight(gl, 0);
        unsafe {
            if reallocate {
                for (texture, width, height) in [
                    (self.y, frame.width, frame.height),
                    (self.u, chroma_width, chroma_height),
                    (self.v, chroma_width, chroma_height),
                ] {
                    gl.bind_texture(glow::TEXTURE_2D, Some(texture));
                    gl.tex_image_2d(
                        glow::TEXTURE_2D,
                        0,
                        glow::R8 as i32,
                        width as i32,
                        height as i32,
                        0,
                        glow::RED,
                        glow::UNSIGNED_BYTE,
                        None,
                    );
                }
            }
            for (texture, width, height, plane) in [
                (self.y, frame.width, frame.height, &frame.y),
                (self.u, chroma_width, chroma_height, &frame.u),
                (self.v, chroma_width, chroma_height, &frame.v),
            ] {
                gl.bind_texture(glow::TEXTURE_2D, Some(texture));
                gl.tex_sub_image_2d(
                    glow::TEXTURE_2D,
                    0,
                    0,
                    0,
                    width as i32,
                    height as i32,
                    glow::RED,
                    glow::UNSIGNED_BYTE,
                    glow::PixelUnpackData::Slice(plane),
                );
            }
        }
        drop(unpack);
        check_gl(gl).context("video upload was rejected by the driver")?;
        self.width = frame.width;
        self.height = frame.height;
        self.uploaded = true;
        Ok(())
    }

    pub(crate) fn destroy(&self, gl: &glow::Context) {
        unsafe {
            gl.delete_texture(self.y);
            gl.delete_texture(self.u);
            gl.delete_texture(self.v);
        }
    }
}

fn upload_scene_texture(
    gl: &glow::Context,
    texture: &Texture,
    wrap: Wrap,
) -> Result<glow::Texture> {
    ensure!(
        texture.image_width > 0 && texture.image_height > 0,
        "scene texture has zero logical dimensions"
    );
    ensure!(
        texture.image_width <= texture.width && texture.image_height <= texture.height,
        "scene texture logical dimensions exceed its stored dimensions"
    );
    let stored_bytes = (texture.width as usize)
        .checked_mul(texture.height as usize)
        .and_then(|n| n.checked_mul(4))
        .context("scene texture size overflow")?;
    ensure!(
        texture.rgba.len() == stored_bytes,
        "scene texture pixels do not match their stored dimensions"
    );
    let row_length = (texture.width != texture.image_width).then_some(texture.width);
    upload_rgba(
        gl,
        texture.image_width,
        texture.image_height,
        row_length,
        &texture.rgba,
        wrap,
        false,
    )
}

fn upload_rgba(
    gl: &glow::Context,
    width: u32,
    height: u32,
    row_length: Option<u32>,
    pixels: &[u8],
    wrap: Wrap,
    mipmaps: bool,
) -> Result<glow::Texture> {
    ensure!(
        width > 0 && height > 0,
        "texture dimensions must be nonzero"
    );
    let row_length = row_length.unwrap_or(width);
    ensure!(row_length >= width, "texture row length is clipped");
    let max_size = unsafe { gl.get_parameter_i32(glow::MAX_TEXTURE_SIZE) };
    ensure!(
        max_size > 0 && width <= max_size as u32 && height <= max_size as u32,
        "texture exceeds GL_MAX_TEXTURE_SIZE ({max_size})"
    );
    ensure!(
        row_length <= i32::MAX as u32,
        "texture row length exceeds GL range"
    );
    let required = (row_length as usize)
        .checked_mul(height as usize)
        .and_then(|n| n.checked_mul(4))
        .context("texture size overflow")?;
    ensure!(
        pixels.len() >= required,
        "texture pixels are shorter than their dimensions"
    );
    let texture = unsafe { gl.create_texture() }.map_err(anyhow::Error::msg)?;
    let unpack = UnpackState::tight(
        gl,
        if row_length == width {
            0
        } else {
            row_length as i32
        },
    );
    unsafe {
        gl.bind_texture(glow::TEXTURE_2D, Some(texture));
        let wrap_mode = match wrap {
            Wrap::Clamp => glow::CLAMP_TO_EDGE,
            Wrap::Repeat => glow::REPEAT,
        };
        gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_S, wrap_mode as i32);
        gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_T, wrap_mode as i32);
        gl.tex_parameter_i32(
            glow::TEXTURE_2D,
            glow::TEXTURE_MAG_FILTER,
            glow::LINEAR as i32,
        );
        gl.tex_parameter_i32(
            glow::TEXTURE_2D,
            glow::TEXTURE_MIN_FILTER,
            if mipmaps {
                glow::LINEAR_MIPMAP_LINEAR as i32
            } else {
                glow::LINEAR as i32
            },
        );
        gl.tex_image_2d(
            glow::TEXTURE_2D,
            0,
            glow::RGBA8 as i32,
            width as i32,
            height as i32,
            0,
            glow::RGBA,
            glow::UNSIGNED_BYTE,
            Some(pixels),
        );
        if mipmaps {
            gl.generate_mipmap(glow::TEXTURE_2D);
        }
        gl.bind_texture(glow::TEXTURE_2D, None);
    }
    drop(unpack);
    if let Err(error) = check_gl(gl) {
        unsafe { gl.delete_texture(texture) };
        return Err(error.context("texture upload was rejected by the driver"));
    }
    Ok(texture)
}

/// Client slices must never inherit a caller's PBO, padding, or pixel offsets.
struct UnpackState<'a> {
    gl: &'a glow::Context,
    values: [i32; 4],
    buffer: Option<glow::Buffer>,
}

impl<'a> UnpackState<'a> {
    const PARAMETERS: [u32; 4] = [
        glow::UNPACK_ALIGNMENT,
        glow::UNPACK_ROW_LENGTH,
        glow::UNPACK_SKIP_ROWS,
        glow::UNPACK_SKIP_PIXELS,
    ];

    fn tight(gl: &'a glow::Context, row_length: i32) -> Self {
        unsafe {
            let values = Self::PARAMETERS.map(|parameter| gl.get_parameter_i32(parameter));
            let buffer = std::num::NonZeroU32::new(
                gl.get_parameter_i32(glow::PIXEL_UNPACK_BUFFER_BINDING) as u32,
            )
            .map(glow::NativeBuffer);
            gl.bind_buffer(glow::PIXEL_UNPACK_BUFFER, None);
            for (parameter, value) in Self::PARAMETERS.into_iter().zip([1, row_length, 0, 0]) {
                gl.pixel_store_i32(parameter, value);
            }
            Self { gl, values, buffer }
        }
    }
}

impl Drop for UnpackState<'_> {
    fn drop(&mut self) {
        unsafe {
            for (parameter, value) in Self::PARAMETERS.into_iter().zip(self.values) {
                self.gl.pixel_store_i32(parameter, value);
            }
            self.gl.bind_buffer(glow::PIXEL_UNPACK_BUFFER, self.buffer);
        }
    }
}

fn check_gl(gl: &glow::Context) -> Result<()> {
    let error = unsafe { gl.get_error() };
    if error != glow::NO_ERROR {
        while unsafe { gl.get_error() } != glow::NO_ERROR {}
        bail!("OpenGL error {error:#x}");
    }
    Ok(())
}

/// A monotone time base shared by GPU scene draws.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Clock {
    started: Instant,
}

impl Clock {
    pub(crate) fn start() -> Self {
        Self {
            started: Instant::now(),
        }
    }

    pub(crate) fn elapsed(&self) -> f32 {
        self.started.elapsed().as_secs_f32()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use khronos_egl as egl;
    use serde_json::json;

    // EGL displays may be shared between contexts, so do not terminate one in
    // parallel with another test using the same display.
    static EGL_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct TestEgl {
        egl: egl::DynamicInstance<egl::EGL1_5>,
        display: egl::Display,
        context: egl::Context,
        gl: glow::Context,
    }

    impl TestEgl {
        fn new(embedded: bool) -> Option<Self> {
            let Some(library) = std::env::var_os("WALLBREAK_TEST_EGL") else {
                eprintln!(
                    "skipping real GPU check without WALLBREAK_TEST_EGL (absolute libEGL.so path)"
                );
                return None;
            };
            assert!(
                std::path::Path::new(&library).is_absolute(),
                "WALLBREAK_TEST_EGL must be an absolute libEGL.so path"
            );
            let egl = unsafe {
                egl::DynamicInstance::<egl::EGL1_5>::load_required_from_filename(&library)
            }
            .expect("cannot load WALLBREAK_TEST_EGL as EGL 1.5");
            // EGL_MESA_platform_surfaceless accepts EGL_DEFAULT_DISPLAY (null).
            let display = unsafe {
                egl.get_platform_display(0x31dd, std::ptr::null_mut(), &[egl::ATTRIB_NONE])
            }
            .expect("EGL_MESA_platform_surfaceless is required for GPU tests");
            egl.initialize(display)
                .expect("cannot initialize surfaceless EGL");
            let (api, bit, attributes) = if embedded {
                (
                    egl::OPENGL_ES_API,
                    egl::OPENGL_ES3_BIT,
                    vec![egl::CONTEXT_MAJOR_VERSION, 3, egl::NONE],
                )
            } else {
                (
                    egl::OPENGL_API,
                    egl::OPENGL_BIT,
                    vec![
                        egl::CONTEXT_MAJOR_VERSION,
                        3,
                        egl::CONTEXT_MINOR_VERSION,
                        2,
                        egl::CONTEXT_OPENGL_PROFILE_MASK,
                        egl::CONTEXT_OPENGL_CORE_PROFILE_BIT,
                        egl::NONE,
                    ],
                )
            };
            egl.bind_api(api).expect("cannot bind requested GL API");
            let config = egl
                .choose_first_config(
                    display,
                    &[
                        egl::SURFACE_TYPE,
                        egl::PBUFFER_BIT,
                        egl::RENDERABLE_TYPE,
                        bit,
                        egl::RED_SIZE,
                        8,
                        egl::GREEN_SIZE,
                        8,
                        egl::BLUE_SIZE,
                        8,
                        egl::ALPHA_SIZE,
                        8,
                        egl::NONE,
                    ],
                )
                .expect("cannot choose EGL config")
                .expect("no EGL config for requested GL API");
            let context = egl
                .create_context(display, config, None, &attributes)
                .expect("cannot create requested GL context");
            egl.make_current(display, None, None, Some(context))
                .expect("cannot make surfaceless context current");
            let gl = unsafe {
                glow::Context::from_loader_function(|name| {
                    egl.get_proc_address(name)
                        .map_or(std::ptr::null(), |p| p as *const _)
                })
            };
            assert_eq!(gl.version().is_embedded, embedded);
            eprintln!(
                "real GPU test: {} / {}",
                unsafe { gl.get_parameter_string(glow::VERSION) },
                unsafe { gl.get_parameter_string(glow::RENDERER) }
            );
            Some(Self {
                egl,
                display,
                context,
                gl,
            })
        }
    }

    impl Drop for TestEgl {
        fn drop(&mut self) {
            self.egl
                .make_current(self.display, None, None, None)
                .unwrap();
            self.egl
                .destroy_context(self.display, self.context)
                .unwrap();
            self.egl.terminate(self.display).unwrap();
        }
    }

    fn texture_pixels(
        gl: &glow::Context,
        texture: glow::Texture,
        width: u32,
        height: u32,
    ) -> Vec<u8> {
        unsafe {
            let framebuffer = gl.create_framebuffer().unwrap();
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(framebuffer));
            gl.framebuffer_texture_2d(
                glow::FRAMEBUFFER,
                glow::COLOR_ATTACHMENT0,
                glow::TEXTURE_2D,
                Some(texture),
                0,
            );
            assert_eq!(
                gl.check_framebuffer_status(glow::FRAMEBUFFER),
                glow::FRAMEBUFFER_COMPLETE
            );
            let mut pixels = vec![0; width as usize * height as usize * 4];
            gl.read_pixels(
                0,
                0,
                width as i32,
                height as i32,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelPackData::Slice(&mut pixels),
            );
            gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            gl.delete_framebuffer(framebuffer);
            check_gl(gl).unwrap();
            pixels
        }
    }

    fn render_pixels(gl: &glow::Context, width: u32, height: u32, draw: impl FnOnce()) -> Vec<u8> {
        let texture = upload_rgba(
            gl,
            width,
            height,
            None,
            &vec![0; width as usize * height as usize * 4],
            Wrap::Clamp,
            false,
        )
        .unwrap();
        unsafe {
            let framebuffer = gl.create_framebuffer().unwrap();
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(framebuffer));
            gl.framebuffer_texture_2d(
                glow::FRAMEBUFFER,
                glow::COLOR_ATTACHMENT0,
                glow::TEXTURE_2D,
                Some(texture),
                0,
            );
            assert_eq!(
                gl.check_framebuffer_status(glow::FRAMEBUFFER),
                glow::FRAMEBUFFER_COMPLETE
            );
            gl.viewport(0, 0, width as i32, height as i32);
            gl.disable(glow::SCISSOR_TEST);
            gl.color_mask(true, true, true, true);
            gl.clear_color(1.0, 0.0, 1.0, 0.25);
            gl.clear(glow::COLOR_BUFFER_BIT);
            draw();
            gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            gl.delete_framebuffer(framebuffer);
        }
        let pixels = texture_pixels(gl, texture, width, height);
        unsafe { gl.delete_texture(texture) };
        // Readback is bottom-up; decoded images and I420 planes are top-down.
        pixels
            .chunks_exact(width as usize * 4)
            .rev()
            .flatten()
            .copied()
            .collect()
    }

    fn assert_pixel(actual: &[u8], expected: [u8; 4]) {
        assert!(
            actual
                .iter()
                .zip(expected)
                .all(|(&a, b)| a.abs_diff(b) <= 2),
            "pixel {actual:?}, expected {expected:?}"
        );
    }

    type TexelPattern = fn(u32, u32) -> [u8; 4];

    fn padded_scene_tex(pixel: TexelPattern) -> Vec<u8> {
        // Logical 5x3 pixels occupy the upper left of a stored 7x5 RGBA mip.
        let mut file = Vec::from(b"TEXV0005\0TEXI0001\0".as_slice());
        for word in [0u32, 0, 7, 5, 5, 3, 0xff123456] {
            file.extend_from_slice(&word.to_le_bytes());
        }
        file.extend_from_slice(b"TEXB0003\0");
        for word in [1u32, u32::MAX, 1, 7, 5, 0, 140, 140] {
            file.extend_from_slice(&word.to_le_bytes());
        }
        for y in 0..5 {
            for x in 0..7 {
                file.extend_from_slice(&if x < 5 && y < 3 {
                    pixel(x, y)
                } else {
                    [255, 0, 255, 255]
                });
            }
        }
        file
    }

    fn real_scene_parity(gl: &glow::Context, renderer: &mut Renderer) -> Vec<String> {
        use crate::scene::MaterialProfile;
        use std::fs;

        let root = tempfile::tempdir().unwrap();
        for directory in ["models", "materials", "shaders"] {
            fs::create_dir(root.path().join(directory)).unwrap();
        }
        let properties = json!({
            "schemecolor": {"type": "color", "value": "0.12 0.28 0.53"},
            "flagcolor1": {"type": "color", "value": "0.61 0.19 0.08"},
            "flagcolor2": {"type": "color", "value": "0.17 0.57 0.32"}
        });
        fs::write(
            root.path().join("project.json"),
            serde_json::to_vec(&json!({
                "type": "scene", "file": "scene.json", "general": {"properties": properties}
            }))
            .unwrap(),
        )
        .unwrap();
        let properties =
            crate::scene::parse_project_colors(&serde_json::from_value(properties).unwrap())
                .unwrap();
        // Parser input, not GLSL compiled by the test: use this project's bias,
        // not the usual 0.5 or a literal copied from a shipped wallpaper.
        fs::write(
            root.path().join("shaders/flowimage.frag"),
            "// flowMask - vec2(0.5, 0.5) is only a comment.\n\
             vec2 flow = (flowColors.rg - vec2(0.29, 0.61)) * 2.0;\n",
        )
        .unwrap();
        let textures: [(&str, TexelPattern); 7] = [
            ("zero", |x, y| {
                [0, 0, 0, (75 + ((x + 2 * y) % 5) * 32) as u8]
            }),
            ("base", |x, y| {
                [
                    (40 + x * 30 + y * 9) as u8,
                    (52 + y * 45 + (x % 2) * 24) as u8,
                    (38 + ((2 * x + 3 * y) % 7) * 25) as u8,
                    (75 + ((x + 2 * y) % 5) * 32) as u8,
                ]
            }),
            ("layer1", |x, y| {
                [
                    (27 + x * 17 + y * 22) as u8,
                    (154 - x * 19 + y * 12) as u8,
                    (52 + ((3 * x + y) % 6) * 23) as u8,
                    (90 + ((2 * x + y) % 5) * 28) as u8,
                ]
            }),
            ("layer2", |x, y| {
                [
                    (162 - x * 23 + y * 8) as u8,
                    (43 + ((x + 3 * y) % 6) * 24) as u8,
                    (44 + x * 23 + y * 15) as u8,
                    (105 + ((x + y) % 5) * 30) as u8,
                ]
            }),
            ("mask", |x, y| {
                [
                    (52 + x * 34 + y * 6) as u8,
                    (193 - y * 49 - x * 9) as u8,
                    (36 + ((x + 2 * y) % 5) * 39) as u8,
                    (63 + ((2 * x + y) % 5) * 31) as u8,
                ]
            }),
            ("normal", |x, y| {
                [
                    (33 + x * 21 + y * 13) as u8,
                    (84 + ((x + 2 * y) % 5) * 23) as u8,
                    (70 + x * 15 + y * 18) as u8,
                    (93 + ((2 * x + y) % 5) * 17) as u8,
                ]
            }),
            ("cloth", |x, y| {
                [
                    (86 + ((2 * x + 3 * y) % 7) * 19) as u8,
                    (31 + x * 27 + y * 11) as u8,
                    (47 + y * 42 + x * 13) as u8,
                    (81 + ((x + y) % 5) * 29) as u8,
                ]
            }),
        ];
        for (name, pixel) in textures {
            fs::write(
                root.path().join(format!("materials/{name}.tex")),
                padded_scene_tex(pixel),
            )
            .unwrap();
        }

        let generic = |texture: &str, blending: &str, constants: serde_json::Value| {
            json!({"shader": "genericimage", "textures": [texture],
                "blending": blending, "constantshadervalues": constants})
        };
        let mut cases = Vec::new();
        for (direction, sign, rotation) in [("forward", 1.0, 0.0), ("reverse", -1.0, 0.43)] {
            for (name, pass) in [
                (
                    "flag",
                    json!({"shader": "flag", "textures": ["base", "normal", "cloth"],
                        "combos": {"TINT": 1},
                        "usershadervalues": {"schemecolor": "color1", "flagcolor1": "color2", "flagcolor2": "color3"},
                        "constantshadervalues": {"Speed": sign * 0.57, "Strength": 0.73}}),
                ),
                (
                    "flowimage_2tex",
                    json!({"shader": "flowimage", "textures": ["base", "mask"],
                        "constantshadervalues": {"Bright": 0.83, "Speed": sign * 0.73, "Amount": 2.7}}),
                ),
                (
                    "flowimage_4tex",
                    json!({"shader": "flowimage", "textures": ["mask", "base", "layer1", "layer2"],
                        "constantshadervalues": {"Bright": 0.91, "Amount": 3.1,
                            "Speed0": sign * 0.73, "Speed1": sign * -0.41, "Speed2": sign * 1.17}}),
                ),
                (
                    "swayimage",
                    json!({"shader": "swayimage", "textures": ["base", "mask"],
                        "constantshadervalues": {"Bright": 0.79, "Speed": sign * 0.37, "Amount": 2.3}}),
                ),
                (
                    "generic_constants",
                    generic(
                        "base",
                        "translucent",
                        json!({"Bright": 0.87, "Alpha": 0.72,
                            "Power": 1.65, "Scroll 1 X": sign * 0.31, "Scroll 1 Y": sign * -0.47}),
                    ),
                ),
            ] {
                cases.push((format!("{name}/{direction}"), vec![pass], rotation, true));
            }
        }
        cases.push((
            "blend_stack".into(),
            vec![
                generic(
                    "base",
                    "opaque",
                    json!({"Bright": 0.61, "Power": 0.8,
                        "Scroll 1 X": 0.29, "Scroll 1 Y": -0.43}),
                ),
                generic(
                    "layer1",
                    "translucent",
                    json!({"Bright": 0.73, "Alpha": 0.64,
                        "Power": 1.3, "Scroll 1 X": -0.37, "Scroll 1 Y": 0.21}),
                ),
                generic(
                    "layer2",
                    "additive",
                    json!({"Bright": 0.42, "Alpha": 0.51,
                        "Scroll 1 X": 0.23, "Scroll 1 Y": -0.33}),
                ),
            ],
            -0.31,
            true,
        ));
        cases.push((
            "bright_translucent_stack".into(),
            vec![
                generic(
                    "base",
                    "translucent",
                    json!({"Bright": 3.8, "Alpha": 0.9,
                        "Scroll 1 X": 0.31, "Scroll 1 Y": -0.47}),
                ),
                generic(
                    "layer2",
                    "translucent",
                    json!({"Bright": 0.28, "Alpha": 0.95,
                        "Scroll 1 X": -0.37, "Scroll 1 Y": 0.21}),
                ),
            ],
            0.43,
            false,
        ));
        for (name, constants) in [
            (
                "generic_alpha_over_one",
                json!({"Bright": 0.8, "Alpha": 3.1}),
            ),
            (
                "generic_alpha_negative",
                json!({"Bright": 0.8, "Alpha": -0.65}),
            ),
            (
                "generic_bright_negative",
                json!({"Bright": -0.4, "Alpha": 0.65, "Power": 1.0}),
            ),
            (
                "generic_negative_squared",
                json!({"Bright": -0.4, "Alpha": 0.65, "Power": 2.0}),
            ),
            (
                "generic_negative_cubed",
                json!({"Bright": -0.4, "Alpha": 0.65, "Power": 3.0}),
            ),
        ] {
            let mut pass = generic("base", "translucent", constants);
            pass["constantshadervalues"]["Scroll 1 X"] = json!(-0.31);
            pass["constantshadervalues"]["Scroll 1 Y"] = json!(0.47);
            cases.push((name.into(), vec![pass], 0.0, false));
        }
        cases.push((
            "generic_zero_power".into(),
            vec![generic(
                "zero",
                "translucent",
                json!({"Power": 0.0, "Alpha": 0.65,
                "Scroll 1 X": -0.31, "Scroll 1 Y": 0.47}),
            )],
            0.0,
            false,
        ));

        let mut failures = Vec::new();
        for (name, passes, rotation, ordinary) in cases {
            let mut objects = Vec::new();
            for (index, mut pass) in passes.into_iter().enumerate() {
                pass["depthtest"] = json!("disabled");
                pass["depthwrite"] = json!("disabled");
                pass["cullmode"] = json!("nocull");
                fs::write(
                    root.path().join(format!("materials/{index}.json")),
                    serde_json::to_vec(&json!({"passes": [pass]})).unwrap(),
                )
                .unwrap();
                fs::write(
                    root.path().join(format!("models/{index}.json")),
                    serde_json::to_vec(&json!({"width": 12, "height": 8,
                        "material": format!("materials/{index}.json")}))
                    .unwrap(),
                )
                .unwrap();
                let (origin, scale, angle) = if name == "blend_stack" && index > 0 {
                    if index == 1 {
                        ("6.47 3.73 0", "0.53 0.48 1", 0.0)
                    } else {
                        ("5.19 4.61 0", "0.43 0.39 1", 0.57)
                    }
                } else {
                    ("5.73 4.11 0", "0.71 0.55 1", rotation)
                };
                objects.push(json!({"id": index + 1, "name": format!("{name}/{index}"),
                    "image": format!("models/{index}.json"), "origin": origin,
                    "scale": scale, "angles": format!("0 0 {angle}")}));
            }
            let scene_path = root.path().join("scene.json");
            fs::write(
                &scene_path,
                serde_json::to_vec(&json!({
                    "camera": {"center": "0 0 -1", "eye": "0 0 0", "up": "0 1 0"},
                    "general": {"bloom": false, "clearcolor": "0.07 0.16 0.31",
                        "orthogonalprojection": {"width": 12, "height": 8}},
                    "objects": objects
                }))
                .unwrap(),
            )
            .unwrap();
            let scene = crate::scene::load(&scene_path, root.path(), &properties).unwrap();
            assert_eq!(scene.profile().layers.len(), objects.len(), "{name}");
            for layer in &scene.profile().layers {
                match &layer.material {
                    MaterialProfile::Flag { colors, .. } => {
                        assert!(name.starts_with("flag/"));
                        assert_eq!(colors.background, properties["schemecolor"]);
                        assert_eq!(colors.accent1, properties["flagcolor1"]);
                        assert_eq!(colors.accent2, properties["flagcolor2"]);
                    }
                    MaterialProfile::Flow { bias, .. } => {
                        assert!(name.starts_with("flowimage_2tex/"));
                        assert_eq!(*bias, [0.29, 0.61]);
                    }
                    MaterialProfile::FlowLayers { bias, .. } => {
                        assert!(name.starts_with("flowimage_4tex/"));
                        assert_eq!(*bias, [0.29, 0.61]);
                    }
                    MaterialProfile::Sway { .. } => assert!(name.starts_with("swayimage/")),
                    MaterialProfile::Generic { .. } => {
                        assert!(name.starts_with("generic_") || name.ends_with("stack"));
                    }
                }
            }
            let gpu_scene = GpuScene::new(gl, &scene).unwrap();
            let mut case_max = [0u8; 4];
            let mut case_mean = [0.0f64; 3];
            let failure_start = failures.len();
            for (width, height) in [(37, 23), (31, 29)] {
                for fit in [Fit::Cover, Fit::Contain] {
                    let mut first = None;
                    let mut animated_cpu = false;
                    let mut animated_gpu = false;
                    for time in [0.0, 0.37, 0.8, 1.6, 2.25] {
                        let mut cpu = scene.render(width, height, fit, time).unwrap();
                        for pixel in cpu.as_chunks_mut::<4>().0 {
                            pixel.swap(0, 2); // Scene::render is BGRA; readback is already top-down RGBA.
                        }
                        let gpu = render_pixels(gl, width, height, || {
                            renderer
                                .draw_scene(gl, &gpu_scene, time, width, height, fit)
                                .unwrap();
                        });
                        check_gl(gl).unwrap();
                        let label = format!("{name} {width}x{height} {fit:?} t={time}");
                        let clear = [18, 41, 79, 255];
                        let colors: std::collections::BTreeSet<_> =
                            cpu.as_chunks::<4>().0.iter().collect();
                        assert!(colors.len() >= 16, "vacuous colors: {label}");
                        assert!(
                            cpu.as_chunks::<4>().0.contains(&clear),
                            "no clear background: {label}"
                        );
                        if ordinary {
                            assert!(
                                cpu.as_chunks::<4>()
                                    .0
                                    .iter()
                                    .all(|p| p[..3].iter().all(|&c| c > 0 && c < 255)),
                                "ordinary fixture unexpectedly saturated: {label}"
                            );
                        }
                        let mut max = [0u8; 4];
                        let mut sum = [0u64; 3];
                        let mut shaded = 0usize;
                        let mut coverage = 0usize;
                        let mut worst = (0u8, 0usize);
                        for (index, (actual, expected)) in gpu
                            .as_chunks::<4>()
                            .0
                            .iter()
                            .zip(cpu.as_chunks::<4>().0)
                            .enumerate()
                        {
                            // Keep every border pixel. Clear/content disagreements are
                            // reported separately, never hidden by a shader tolerance.
                            coverage += usize::from((*actual == clear) != (*expected == clear));
                            shaded += usize::from(*expected != clear);
                            for channel in 0..4 {
                                let error = actual[channel].abs_diff(expected[channel]);
                                max[channel] = max[channel].max(error);
                                if channel < 3 && *expected != clear {
                                    sum[channel] += u64::from(error);
                                }
                                if error > worst.0 {
                                    worst = (error, index);
                                }
                            }
                        }
                        assert!(
                            shaded >= (width * height / 10) as usize,
                            "vacuous coverage: {label}"
                        );
                        let mean = sum.map(|n| n as f64 / shaded as f64);
                        for channel in 0..4 {
                            case_max[channel] = case_max[channel].max(max[channel]);
                            if channel < 3 {
                                case_mean[channel] = case_mean[channel].max(mean[channel]);
                            }
                        }
                        // Four UNORM8 steps allow hardware bilinear precision,
                        // shader arithmetic and final rounding, not semantic drift.
                        // Mean is per RGB channel over content, not diluted by clear.
                        if coverage != 0
                            || max[..3].iter().any(|&n| n > 4)
                            || max[3] != 0
                            || mean.iter().any(|&n| n > 0.75)
                        {
                            let offset = worst.1 * 4;
                            failures.push(format!("{label}: max_rgba={max:?}, mean_rgb={mean:.3?}, clear/content_mismatches={coverage}, worst=({}, {}) cpu={:?} gpu={:?}",
                                worst.1 % width as usize, worst.1 / width as usize,
                                &cpu[offset..offset + 4], &gpu[offset..offset + 4]));
                        }
                        if let Some((cpu_first, gpu_first)) = &first {
                            animated_cpu |= cpu != *cpu_first;
                            animated_gpu |= gpu != *gpu_first;
                        } else {
                            first = Some((cpu, gpu));
                        }
                    }
                    assert!(
                        animated_cpu,
                        "vacuous animation: {name} {width}x{height} {fit:?}"
                    );
                    if !animated_gpu {
                        failures.push(format!(
                            "{name} {width}x{height} {fit:?}: GPU animation is constant"
                        ));
                    }
                }
            }
            eprintln!(
                "scene parity {name}: 20 frames, max_rgba={case_max:?}, max_mean_rgb={case_mean:.3?}, failures={}",
                failures.len() - failure_start
            );
            gpu_scene.destroy(gl);
        }
        failures
    }

    fn scene_target_regressions(gl: &glow::Context, renderer: &mut Renderer) {
        let scene = GpuScene {
            clear: [0.2, 0.4, 0.6],
            projection: [3, 2],
            layers: Vec::new(),
        };
        let pixels = render_pixels(gl, 8, 6, || {
            let draw = unsafe { gl.get_parameter_i32(glow::DRAW_FRAMEBUFFER_BINDING) };
            let read = unsafe { gl.create_framebuffer() }.unwrap();
            unsafe {
                gl.bind_framebuffer(glow::READ_FRAMEBUFFER, Some(read));
                gl.viewport(2, 1, 4, 3);
                gl.enable(glow::SCISSOR_TEST);
                gl.scissor(3, 1, 2, 3);
                gl.color_mask(false, true, false, false);
            }
            let assert_state = |viewport: [i32; 4]| unsafe {
                assert_eq!(gl.get_parameter_i32(glow::DRAW_FRAMEBUFFER_BINDING), draw);
                assert_eq!(
                    gl.get_parameter_i32(glow::READ_FRAMEBUFFER_BINDING) as u32,
                    read.0.get()
                );
                let mut actual = [0; 4];
                gl.get_parameter_i32_slice(glow::VIEWPORT, &mut actual);
                assert_eq!(actual, viewport);
                gl.get_parameter_i32_slice(glow::SCISSOR_BOX, &mut actual);
                assert_eq!(actual, [3, 1, 2, 3]);
                assert!(gl.is_enabled(glow::SCISSOR_TEST));
                gl.get_parameter_i32_slice(glow::COLOR_WRITEMASK, &mut actual);
                assert_eq!(actual, [0, 1, 0, 0]);
            };
            renderer
                .draw_scene(gl, &scene, 0.0, 4, 3, Fit::Contain)
                .unwrap();
            assert_state([2, 1, 4, 3]);
            let cached = renderer.scene_target.as_ref().unwrap().texture;
            renderer
                .draw_scene(gl, &scene, 0.25, 4, 3, Fit::Cover)
                .unwrap();
            assert_eq!(renderer.scene_target.as_ref().unwrap().texture, cached);
            assert_state([2, 1, 4, 3]);

            assert!(
                renderer
                    .draw_scene(gl, &scene, 0.0, 4097, 4096, Fit::Cover)
                    .is_err()
            );
            assert_eq!(renderer.scene_target.as_ref().unwrap().texture, cached);
            assert_state([2, 1, 4, 3]);
            let max_size = unsafe { gl.get_parameter_i32(glow::MAX_TEXTURE_SIZE) } as u32;
            assert!(
                renderer
                    .draw_scene(gl, &scene, 0.0, max_size + 1, 1, Fit::Cover)
                    .is_err()
            );
            assert_state([2, 1, 4, 3]);

            // Allocation failures restore caller state and do not poison a retry.
            unsafe { gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 3) };
            assert!(
                renderer
                    .draw_scene(gl, &scene, 0.0, 5, 4, Fit::Cover)
                    .is_err()
            );
            assert_state([2, 1, 4, 3]);
            check_gl(gl).unwrap();
            unsafe { gl.viewport(1, 1, 6, 4) };
            renderer
                .draw_scene(gl, &scene, 0.0, 6, 4, Fit::Cover)
                .unwrap();
            let target = renderer.scene_target.as_ref().unwrap();
            assert_eq!((target.width, target.height), (6, 4));
            assert_state([1, 1, 6, 4]);
            unsafe {
                gl.bind_framebuffer(glow::READ_FRAMEBUFFER, None);
                gl.delete_framebuffer(read);
            }
        });
        for y in 0..6usize {
            for x in 0..8usize {
                let expected = if (3..5).contains(&x) && (1..4).contains(&(5 - y)) {
                    [51, 102, 153, 255]
                } else {
                    [255, 0, 255, 64]
                };
                assert_pixel(&pixels[(y * 8 + x) * 4..][..4], expected);
            }
        }
    }

    fn real_gpu_regressions(embedded: bool) {
        let _lock = EGL_LOCK.lock().unwrap();
        let Some(context) = TestEgl::new(embedded) else {
            return;
        };
        let gl = &context.gl;
        // Exercise compilation and missing-uniform failure paths before creating
        // the renderer, which compiles and links all seven production programs.
        assert!(Program::new(gl, "not valid GLSL", &[]).is_err());
        let link_error = Program::new(
            gl,
            "in vec3 v_uv; out vec4 frag_color; void main() { frag_color = vec4(v_uv, 1.0); }",
            &[],
        )
        .err()
        .expect("mismatched vertex/fragment varyings must fail linking");
        assert!(link_error.to_string().contains("program link failed"));
        assert!(Program::new(gl, IMAGE_FRAGMENT, &["u_missing"]).is_err());
        let mut renderer = Renderer::new(gl).unwrap();
        assert!(
            renderer.scene_target.is_none(),
            "images and video need no float target"
        );
        check_gl(gl).unwrap();

        let pbo = unsafe {
            let pbo = gl.create_buffer().unwrap();
            gl.bind_buffer(glow::PIXEL_UNPACK_BUFFER, Some(pbo));
            gl.buffer_data_u8_slice(glow::PIXEL_UNPACK_BUFFER, &[0; 64], glow::STATIC_DRAW);
            for (parameter, value) in UnpackState::PARAMETERS.into_iter().zip([8, 19, 2, 3]) {
                gl.pixel_store_i32(parameter, value);
            }
            pbo
        };
        let assert_unpack = || unsafe {
            for (parameter, expected) in UnpackState::PARAMETERS.into_iter().zip([8, 19, 2, 3]) {
                assert_eq!(gl.get_parameter_i32(parameter), expected);
            }
            assert_eq!(
                gl.get_parameter_i32(glow::PIXEL_UNPACK_BUFFER_BINDING) as u32,
                pbo.0.get()
            );
        };

        let source = RgbaImage::from_fn(4, 2, |x, _| {
            image::Rgba(match x {
                0 => [255, 0, 0, 255],
                3 => [0, 0, 255, 255],
                _ => [0, 255, 0, 255],
            })
        });
        let image = GpuImage::new(gl, &source).unwrap();
        assert_unpack();
        let contain = render_pixels(gl, 4, 4, || {
            renderer.draw_image(gl, &image, 4, 4, Fit::Contain).unwrap()
        });
        for y in 0..4usize {
            for x in 0..4usize {
                let expected = if y == 0 || y == 3 {
                    [0, 0, 0, 255]
                } else {
                    source.get_pixel(x as u32, 0).0
                };
                assert_pixel(&contain[(y * 4 + x) * 4..][..4], expected);
            }
        }
        let cover = render_pixels(gl, 4, 4, || {
            renderer.draw_image(gl, &image, 4, 4, Fit::Cover).unwrap()
        });
        for row in cover.as_chunks::<16>().0 {
            for (pixel, expected) in row.as_chunks::<4>().0.iter().zip([
                [64, 191, 0, 255],
                [0, 255, 0, 255],
                [0, 255, 0, 255],
                [0, 191, 64, 255],
            ]) {
                assert_pixel(pixel, expected);
            }
        }
        image.destroy(gl);
        let image = GpuImage::new(
            gl,
            &RgbaImage::from_fn(2, 2, |x, y| {
                image::Rgba(if x == y {
                    [200, 100, 50, 128]
                } else {
                    [255, 0, 255, 0]
                })
            }),
        )
        .unwrap();
        let pixels = render_pixels(gl, 2, 2, || {
            renderer.draw_image(gl, &image, 2, 2, Fit::Cover).unwrap()
        });
        for (pixel, expected) in pixels.as_chunks::<4>().0.iter().zip([
            [100, 50, 25, 255],
            [0, 0, 0, 255],
            [0, 0, 0, 255],
            [100, 50, 25, 255],
        ]) {
            assert_pixel(pixel, expected);
        }
        image.destroy(gl);

        // Logical 2x2 data in padded 3x3 storage must not sample the padding.
        let padded = Texture {
            width: 3,
            height: 3,
            image_width: 2,
            image_height: 2,
            format: 0,
            flags: 0,
            rgba: vec![
                200, 0, 0, 128, 0, 200, 0, 128, 255, 0, 255, 255, 0, 0, 200, 128, 200, 200, 0, 128,
                255, 0, 255, 255, 255, 0, 255, 255, 255, 0, 255, 255, 255, 0, 255, 255,
            ],
        };
        for (blending, expected) in [
            (Blending::Opaque, [200, 0, 0, 255]),
            (Blending::Translucent, [100, 0, 0, 255]),
            (Blending::Additive, [100, 0, 0, 255]),
        ] {
            let texture = upload_scene_texture(gl, &padded, Wrap::Repeat).unwrap();
            assert_unpack();
            let scene = GpuScene {
                clear: [0.0; 3],
                projection: [2, 2],
                layers: vec![GpuLayer {
                    geometry: layer([1.0, 1.0], [2.0, 2.0], 0.0),
                    blending,
                    material: GpuMaterial::Generic {
                        texture,
                        brightness: 1.0,
                        alpha: 1.0,
                        power: 1.0,
                        scroll: [0.0; 2],
                    },
                }],
            };
            let pixels = render_pixels(gl, 2, 2, || {
                unsafe { gl.color_mask(false, true, false, false) };
                renderer
                    .draw_scene(gl, &scene, 0.0, 2, 2, Fit::Cover)
                    .unwrap();
                let mut mask = [0; 4];
                unsafe { gl.get_parameter_i32_slice(glow::COLOR_WRITEMASK, &mut mask) };
                assert_eq!(mask, [0, 1, 0, 0]);
            });
            assert_pixel(&pixels[..4], expected);
            assert_pixel(&pixels[4..8], [0, expected[0], 0, 255]);
            assert_pixel(&pixels[8..12], [0, 0, expected[0], 255]);
            assert_pixel(&pixels[12..16], [expected[0], expected[0], 0, 255]);
            scene.destroy(gl);
        }

        let mut video = GpuVideo::new(gl).unwrap();
        for (width, height) in [(6, 4), (3, 3), (6, 4)] {
            let mut frame = Yuv420Frame {
                width,
                height,
                y: (0..width * height).map(|n| 16 + n as u8 * 7).collect(),
                u: (0..width.div_ceil(2) * height.div_ceil(2))
                    .map(|n| 100 + n as u8 * 5)
                    .collect(),
                v: (0..width.div_ceil(2) * height.div_ceil(2))
                    .map(|n| 150 - n as u8 * 5)
                    .collect(),
            };
            video.upload(gl, &frame).unwrap();
            assert_unpack();
            for (texture, w, h, plane) in [
                (video.y, width, height, &frame.y),
                (video.u, width.div_ceil(2), height.div_ceil(2), &frame.u),
                (video.v, width.div_ceil(2), height.div_ceil(2), &frame.v),
            ] {
                let pixels = texture_pixels(gl, texture, w, h);
                assert_eq!(
                    pixels
                        .as_chunks::<4>()
                        .0
                        .iter()
                        .map(|p| p[0])
                        .collect::<Vec<_>>(),
                    *plane
                );
            }
            frame.u.fill(128);
            frame.v.fill(128);
            video.upload(gl, &frame).unwrap();
            let pixels = render_pixels(gl, width, height, || {
                renderer.draw_video(gl, &video).unwrap()
            });
            for (pixel, &y) in pixels.as_chunks::<4>().0.iter().zip(&frame.y) {
                let gray = ((f32::from(y) - 16.0) * 255.0 / 219.0).round() as u8;
                assert_pixel(pixel, [gray, gray, gray, 255]);
            }
            // Break only the final plane's storage: Y/U update, V then fails.
            {
                let _unpack = UnpackState::tight(gl, 0);
                unsafe {
                    gl.bind_texture(glow::TEXTURE_2D, Some(video.v));
                    gl.tex_image_2d(
                        glow::TEXTURE_2D,
                        0,
                        glow::R8 as i32,
                        1,
                        1,
                        0,
                        glow::RED,
                        glow::UNSIGNED_BYTE,
                        None,
                    );
                }
            }
            check_gl(gl).unwrap();
            assert!(video.upload(gl, &frame).is_err());
            assert!(!video.uploaded);
            assert!(renderer.draw_video(gl, &video).is_err());
            assert_unpack();
            video.upload(gl, &frame).unwrap();
            assert_eq!(
                render_pixels(gl, width, height, || renderer
                    .draw_video(gl, &video)
                    .unwrap()),
                pixels
            );
        }

        let max_size = unsafe { gl.get_parameter_i32(glow::MAX_TEXTURE_SIZE) } as u32;
        for (width, height) in [
            (0, 1),
            (6, 4),
            (8192, 8192),
            (max_size + 1, 1),
            (u32::MAX, u32::MAX),
        ] {
            let frame = Yuv420Frame {
                width,
                height,
                y: vec![],
                u: vec![],
                v: vec![],
            };
            assert!(video.upload(gl, &frame).is_err());
            assert!(!video.uploaded);
            assert!(renderer.draw_video(gl, &video).is_err());
            assert_unpack();
            let good = Yuv420Frame {
                width: 3,
                height: 3,
                y: vec![235; 9],
                u: vec![128; 4],
                v: vec![128; 4],
            };
            video.upload(gl, &good).unwrap();
            for pixel in render_pixels(gl, 3, 3, || renderer.draw_video(gl, &video).unwrap())
                .as_chunks::<4>()
                .0
            {
                assert_pixel(pixel, [255; 4]);
            }
        }
        assert!(upload_rgba(gl, max_size + 1, 1, None, &[], Wrap::Clamp, false).is_err());
        assert!(upload_rgba(gl, 2, 2, None, &[0; 4], Wrap::Clamp, false).is_err());
        // Pending GL errors fail closed, and the error path restores unpack
        // state just like a successful upload (without poisoning the retry).
        unsafe { gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 3) };
        assert!(upload_rgba(gl, 1, 1, None, &[255; 4], Wrap::Clamp, false).is_err());
        assert_unpack();
        let recovered = upload_rgba(gl, 1, 1, None, &[255; 4], Wrap::Clamp, false).unwrap();
        assert_eq!(texture_pixels(gl, recovered, 1, 1), [255; 4]);
        unsafe { gl.delete_texture(recovered) };
        assert_unpack();
        video.destroy(gl);
        let parity_failures = real_scene_parity(gl, &mut renderer);
        scene_target_regressions(gl, &mut renderer);
        assert_unpack();
        let target = renderer.scene_target.as_ref().unwrap();
        let target_handles = (target.framebuffer, target.texture);
        renderer.destroy(gl);
        unsafe {
            assert!(!gl.is_framebuffer(target_handles.0));
            assert!(!gl.is_texture(target_handles.1));
            gl.bind_buffer(glow::PIXEL_UNPACK_BUFFER, None);
            gl.delete_buffer(pbo);
        }
        check_gl(gl).unwrap();
        drop(context);
        drop(_lock);
        assert!(
            parity_failures.is_empty(),
            "{} scene parity failures:\n{}",
            parity_failures.len(),
            parity_failures.join("\n")
        );
    }

    #[test]
    #[ignore = "requires WALLBREAK_TEST_EGL pointing to an absolute EGL 1.5 library with Mesa surfaceless support"]
    fn real_surfaceless_desktop_gpu() {
        real_gpu_regressions(false);
    }

    #[test]
    #[ignore = "requires WALLBREAK_TEST_EGL pointing to an absolute EGL 1.5 library with Mesa surfaceless support"]
    fn real_surfaceless_gles3_gpu() {
        real_gpu_regressions(true);
    }

    fn layer(center: [f32; 2], extents: [f32; 2], rotation: f32) -> LayerGeometry {
        LayerGeometry {
            center,
            extents,
            rotation,
        }
    }

    /// Placement must be the exact inverse of the CPU `layer_uv` mapping.
    #[test]
    fn placement_inverts_the_cpu_layer_mapping() {
        for rotation in [0.0f32, 0.7, -1.9, std::f32::consts::PI] {
            let geometry = layer([1.3, -0.4], [2.5, 1.1], rotation);
            let layout = scene_layout([6, 2], 96, 32, Fit::Cover).unwrap();
            let placement = layer_placement(&layout, &geometry, 96, 32);
            for (u, v) in [(0.0f64, 0.0f64), (1.0, 1.0), (0.25, 0.75)] {
                let point = [
                    placement.origin[0]
                        + (u as f32) * placement.axis_u[0]
                        + (v as f32) * placement.axis_v[0],
                    placement.origin[1]
                        + (u as f32) * placement.axis_u[1]
                        + (v as f32) * placement.axis_v[1],
                ];
                let x = ((f64::from(point[0]) + 1.0) / 2.0 * 96.0) as f64;
                let y = ((1.0 - f64::from(point[1])) / 2.0 * 32.0) as f64;
                let scene_x = (x - layout.left) / layout.factor;
                let scene_y = (y - layout.top) / layout.factor;
                let dx = scene_x - f64::from(geometry.center[0]);
                let dy = scene_y - f64::from(geometry.center[1]);
                let (sin, cos) = f64::from(rotation).sin_cos();
                let recovered_u = (dx * cos + dy * sin) / f64::from(geometry.extents[0]) + 0.5;
                let recovered_v = (dy * cos - dx * sin) / f64::from(geometry.extents[1]) + 0.5;
                assert!(
                    (recovered_u - u).abs() < 1e-4 && (recovered_v - v).abs() < 1e-4,
                    "rotation {rotation}: ({u}, {v}) -> ({recovered_u}, {recovered_v})"
                );
            }
        }
    }

    #[test]
    fn image_placement_fills_or_letterboxes() {
        let cover = image_placement((4, 2), 100, 100, Fit::Cover).unwrap();
        // Cover overscans horizontally; interpolation crops to the center.
        assert_eq!(cover.origin, [-2.0, 1.0]);
        assert_eq!(cover.axis_u, [4.0, 0.0]);
        assert_eq!(cover.axis_v, [0.0, -2.0]);
        let contain = image_placement((4, 2), 100, 100, Fit::Contain).unwrap();
        // Centered band of 100x50 pixels.
        assert!((contain.origin[1] - 0.5).abs() < 1e-6);
        assert!((contain.axis_v[1] + 1.0).abs() < 1e-6);
        assert!((contain.axis_u[0] - 2.0).abs() < 1e-6);
        assert!(image_placement((0, 2), 10, 10, Fit::Cover).is_err());
    }
}
