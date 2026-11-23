//! Draws NV12 video frames with a small OpenGL shader, as part of egui's
//! normal painting (a paint callback).
//!
//! FFmpeg hands us frames in the decoder's own YUV layout; the GPU turns them
//! into RGB while drawing. That saves FFmpeg its most expensive step (RGB
//! conversion of 3K video) and uploads 1.5 bytes per pixel instead of 4.
//! The colour maths comes from `tapestry_core::ColorInfo`, which the core
//! tests check against FFmpeg's own RGB output.

use eframe::egui;
use eframe::egui_glow;
use eframe::glow::{self, HasContext};
use std::sync::{Arc, Mutex};
use tapestry_core::{ColorInfo, Frame};

/// One video's frame on the GPU: a full-size Y texture and a half-size
/// interleaved U/V texture.
pub struct VideoTexture {
    y: glow::Texture,
    uv: glow::Texture,
    size: [u32; 2],
    color: ColorInfo,
    /// Freed by the renderer on the next frame (a texture dropped mid-frame
    /// may still be drawn by this frame's paint callbacks).
    trash: Arc<Mutex<Vec<glow::Texture>>>,
}

impl Drop for VideoTexture {
    fn drop(&mut self) {
        self.trash.lock().unwrap().extend([self.y, self.uv]);
    }
}

pub struct VideoRenderer {
    program: glow::Program,
    vao: glow::VertexArray,
    u_y: Option<glow::UniformLocation>,
    u_uv: Option<glow::UniformLocation>,
    u_uv_rect: Option<glow::UniformLocation>,
    u_matrix: Option<glow::UniformLocation>,
    u_offset: Option<glow::UniformLocation>,
    u_chroma_shift: Option<glow::UniformLocation>,
    trash: Arc<Mutex<Vec<glow::Texture>>>,
}

const VERTEX: &str = r#"
in vec2 a_pos;            // corner of the rectangle, 0..1
uniform vec4 u_uv_rect;   // visible part of the frame: min.xy, max.xy
out vec2 v_uv;
void main() {
    v_uv = mix(u_uv_rect.xy, u_uv_rect.zw, a_pos);
    gl_Position = vec4(a_pos.x * 2.0 - 1.0, 1.0 - a_pos.y * 2.0, 0.0, 1.0);
}
"#;

const FRAGMENT: &str = r#"
uniform sampler2D u_y;
uniform sampler2D u_uv;
uniform mat3 u_matrix;
uniform vec3 u_offset;
uniform vec2 u_chroma_shift;
in vec2 v_uv;
out vec4 out_color;
void main() {
    float y = texture(u_y, v_uv).r;
    vec2 c = texture(u_uv, v_uv + u_chroma_shift).rg;
    out_color = vec4(clamp(u_matrix * vec3(y, c) + u_offset, 0.0, 1.0), 1.0);
}
"#;

impl VideoRenderer {
    pub fn new(gl: &glow::Context) -> Result<Self, String> {
        let version = egui_glow::ShaderVersion::get(gl);
        if !version.is_new_shader_interface() {
            return Err(format!("video needs OpenGL 3 or newer (this PC offers {:?})", gl.version()));
        }
        let header = format!("{}#ifdef GL_ES\nprecision highp float;\n#endif\n", version.version_declaration());
        unsafe {
            let program = gl.create_program()?;
            let mut shaders = Vec::new();
            for (kind, src) in [(glow::VERTEX_SHADER, VERTEX), (glow::FRAGMENT_SHADER, FRAGMENT)] {
                let s = gl.create_shader(kind)?;
                gl.shader_source(s, &format!("{header}{src}"));
                gl.compile_shader(s);
                if !gl.get_shader_compile_status(s) {
                    return Err(format!("video shader: {}", gl.get_shader_info_log(s)));
                }
                gl.attach_shader(program, s);
                shaders.push(s);
            }
            gl.link_program(program);
            if !gl.get_program_link_status(program) {
                return Err(format!("video shader: {}", gl.get_program_info_log(program)));
            }
            for s in shaders {
                gl.detach_shader(program, s);
                gl.delete_shader(s);
            }

            // A unit square, drawn as a triangle strip.
            let corners: [f32; 8] = [0.0, 0.0, 1.0, 0.0, 0.0, 1.0, 1.0, 1.0];
            let vao = gl.create_vertex_array()?;
            let vbo = gl.create_buffer()?;
            gl.bind_vertex_array(Some(vao));
            gl.bind_buffer(glow::ARRAY_BUFFER, Some(vbo));
            let bytes: Vec<u8> = corners.iter().flat_map(|f| f.to_ne_bytes()).collect();
            gl.buffer_data_u8_slice(glow::ARRAY_BUFFER, &bytes, glow::STATIC_DRAW);
            let a_pos = gl.get_attrib_location(program, "a_pos").ok_or("video shader: no a_pos")?;
            gl.enable_vertex_attrib_array(a_pos);
            gl.vertex_attrib_pointer_f32(a_pos, 2, glow::FLOAT, false, 8, 0);
            gl.bind_vertex_array(None);
            gl.bind_buffer(glow::ARRAY_BUFFER, None);

            let u = |name| gl.get_uniform_location(program, name);
            Ok(VideoRenderer {
                program,
                vao,
                u_y: u("u_y"),
                u_uv: u("u_uv"),
                u_uv_rect: u("u_uv_rect"),
                u_matrix: u("u_matrix"),
                u_offset: u("u_offset"),
                u_chroma_shift: u("u_chroma_shift"),
                trash: Arc::new(Mutex::new(Vec::new())),
            })
        }
    }

    /// Free textures dropped since the last frame. Call once per frame, before painting.
    pub fn free_unused(&self, gl: &glow::Context) {
        for t in std::mem::take(&mut *self.trash.lock().unwrap()) {
            unsafe { gl.delete_texture(t) };
        }
    }

    /// Copy `frame` to `tex`, (re)creating it if the size changed.
    pub fn upload(&self, gl: &glow::Context, tex: &mut Option<VideoTexture>, frame: &Frame) {
        let size = [frame.width, frame.height];
        if tex.as_ref().map_or(true, |t| t.size != size) {
            *tex = None; // trashes the old one
            match unsafe { self.create_texture(gl, size) } {
                Ok(t) => *tex = Some(t),
                Err(_) => return,
            }
        }
        let Some(t) = tex.as_mut() else { return };
        t.color = frame.color;
        let (w, h) = (frame.width as i32, frame.height as i32);
        unsafe {
            gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 1);
            gl.bind_texture(glow::TEXTURE_2D, Some(t.y));
            let y = glow::PixelUnpackData::Slice(frame.y_plane());
            gl.tex_sub_image_2d(glow::TEXTURE_2D, 0, 0, 0, w, h, glow::RED, glow::UNSIGNED_BYTE, y);
            gl.bind_texture(glow::TEXTURE_2D, Some(t.uv));
            let uv = glow::PixelUnpackData::Slice(frame.uv_plane());
            gl.tex_sub_image_2d(glow::TEXTURE_2D, 0, 0, 0, w / 2, h / 2, glow::RG, glow::UNSIGNED_BYTE, uv);
            gl.bind_texture(glow::TEXTURE_2D, None);
        }
    }

    unsafe fn create_texture(&self, gl: &glow::Context, [w, h]: [u32; 2]) -> Result<VideoTexture, String> {
        let make = |internal: u32, format: u32, w: u32, h: u32| -> Result<glow::Texture, String> {
            let t = gl.create_texture()?;
            gl.bind_texture(glow::TEXTURE_2D, Some(t));
            for (p, v) in [
                (glow::TEXTURE_MIN_FILTER, glow::LINEAR),
                (glow::TEXTURE_MAG_FILTER, glow::LINEAR),
                (glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE),
                (glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE),
            ] {
                gl.tex_parameter_i32(glow::TEXTURE_2D, p, v as i32);
            }
            gl.tex_image_2d(glow::TEXTURE_2D, 0, internal as i32, w as i32, h as i32, 0, format, glow::UNSIGNED_BYTE, None);
            Ok(t)
        };
        let y = make(glow::R8, glow::RED, w, h)?;
        let uv = make(glow::RG8, glow::RG, w / 2, h / 2)?;
        gl.bind_texture(glow::TEXTURE_2D, None);
        Ok(VideoTexture { y, uv, size: [w, h], color: ColorInfo::default(), trash: self.trash.clone() })
    }

    unsafe fn draw(&self, gl: &glow::Context, y: glow::Texture, uv: glow::Texture, width: u32, uv_rect: egui::Rect, color: ColorInfo) {
        let (m, off) = color.rgb_transform();
        gl.use_program(Some(self.program));
        gl.bind_vertex_array(Some(self.vao));
        gl.active_texture(glow::TEXTURE0);
        gl.bind_texture(glow::TEXTURE_2D, Some(y));
        gl.active_texture(glow::TEXTURE1);
        gl.bind_texture(glow::TEXTURE_2D, Some(uv));
        gl.uniform_1_i32(self.u_y.as_ref(), 0);
        gl.uniform_1_i32(self.u_uv.as_ref(), 1);
        gl.uniform_4_f32(self.u_uv_rect.as_ref(), uv_rect.min.x, uv_rect.min.y, uv_rect.max.x, uv_rect.max.y);
        gl.uniform_matrix_3_f32_slice(self.u_matrix.as_ref(), false, &m);
        gl.uniform_3_f32(self.u_offset.as_ref(), off[0], off[1], off[2]);
        // H.264's default chroma siting puts each U/V sample on the left of
        // its two luma columns, not between them: shift a quarter chroma texel.
        gl.uniform_2_f32(self.u_chroma_shift.as_ref(), 0.5 / width.max(1) as f32, 0.0);
        gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
        gl.bind_texture(glow::TEXTURE_2D, None);
        gl.active_texture(glow::TEXTURE0);
        // egui restores the rest of its state after every callback.
    }
}

/// Draw `tex` into `rect`, showing the `uv` part of the frame (0..1 coordinates).
pub fn paint(renderer: Option<&Arc<VideoRenderer>>, painter: &egui::Painter, rect: egui::Rect, uv: egui::Rect, tex: &VideoTexture) {
    let Some(r) = renderer.cloned() else { return };
    let (y, uv_tex, width, color) = (tex.y, tex.uv, tex.size[0], tex.color);
    painter.add(egui::PaintCallback {
        rect,
        callback: Arc::new(egui_glow::CallbackFn::new(move |_info, p| unsafe {
            r.draw(p.gl(), y, uv_tex, width, uv, color);
        })),
    });
}
