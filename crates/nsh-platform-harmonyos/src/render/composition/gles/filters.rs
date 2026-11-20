//! Ordered CSS passes over premultiplied GPU pictures. Intermediate attachments
//! belong to this pipeline; no native pixels cross to the CPU.
use std::{cell::Cell, rc::Rc};

use glow::HasContext;
use nsh_core::Filter;

use super::{Error, Picture, Result, Target, TextureContext};

const VERTEX: &str = "#version 300 es\nvoid main() { vec2 p = vec2(float((gl_VertexID << 1) & 2), float(gl_VertexID & 2)); gl_Position = vec4(p * 2.0 - 1.0, 0.0, 1.0); }";
const FRAGMENT: &str = r#"#version 300 es
precision highp float;
uniform sampler2D source;
uniform sampler2D shadow;
uniform vec2 source_size;
uniform vec2 source_scale;
uniform vec2 shadow_size;
uniform vec2 offset;
uniform vec2 shadow_offset;
uniform vec2 direction;
uniform float sigma;
uniform int mode;
uniform mat4 matrix;
uniform vec4 bias;
uniform vec4 color;
out vec4 result;
vec4 sample_source(vec2 p) {
    if (any(lessThan(p, vec2(0.0))) || any(greaterThanEqual(p, source_size))) return vec4(0.0);
    return texture(source, p / source_size);
}
void main() {
    vec2 p = (gl_FragCoord.xy - offset) * source_scale;
    vec4 c = sample_source(p);
    if (mode == 0) {
        if (c.a > 0.0) c.rgb /= c.a;
        c = clamp(matrix * c + bias, 0.0, 1.0);
        result = vec4(c.rgb * c.a, c.a);
    } else if (mode == 1) {
        if (sigma <= 0.0) { result = c; return; }
        float sum = 1.0;
        int radius = int(ceil(sigma * 3.0));
        for (int i = 1; i <= radius; i += 2) {
            float a = float(i), b = float(min(i + 1, radius));
            float wa = exp(-0.5 * a * a / (sigma * sigma));
            float wb = i + 1 <= radius ? exp(-0.5 * b * b / (sigma * sigma)) : 0.0;
            float w = wa + wb;
            vec2 d = direction * ((a * wa + b * wb) / w);
            c += w * (sample_source(p + d) + sample_source(p - d));
            sum += 2.0 * w;
        }
        result = c / sum;
    } else if (mode == 2) {
        vec2 q = gl_FragCoord.xy - shadow_offset;
        float alpha = 0.0;
        if (all(greaterThanEqual(q, vec2(0.0))) && all(lessThan(q, shadow_size))) alpha = texture(shadow, q / shadow_size).a;
        alpha *= color.a;
        result = c + vec4(color.rgb * alpha, alpha) * (1.0 - c.a);
    } else if (mode == 4) {
        result = c + texture(shadow, gl_FragCoord.xy / source_size) * (1.0 - c.a);
    } else { result = c; }
}
"#;

struct Pass {
    mode: i32,
    size: (u32, u32),
    offset: [f32; 2],
    direction: [f32; 2],
    source_scale: [f32; 2],
    sigma: f32,
    matrix: [f32; 20],
    shadow: Option<(Picture, [f32; 2], [f32; 4])>,
}
impl Pass {
    fn new(size: (u32, u32)) -> Self {
        Self {
            mode: 0,
            size,
            offset: [0.0; 2],
            direction: [0.0; 2],
            source_scale: [1.0; 2],
            sigma: 0.0,
            matrix: Filter::Brightness(1.0).color_matrix().unwrap(),
            shadow: None,
        }
    }
}

pub(in crate::render::composition) struct FilterProgram {
    context: Rc<super::super::readback::Context>,
    gl: Rc<glow::Context>,
    program: glow::Program,
    warmed: Cell<bool>,
    source: Option<glow::UniformLocation>,
    shadow: Option<glow::UniformLocation>,
    source_size: Option<glow::UniformLocation>,
    source_scale: Option<glow::UniformLocation>,
    shadow_size: Option<glow::UniformLocation>,
    offset: Option<glow::UniformLocation>,
    shadow_offset: Option<glow::UniformLocation>,
    direction: Option<glow::UniformLocation>,
    sigma: Option<glow::UniformLocation>,
    mode: Option<glow::UniformLocation>,
    matrix: Option<glow::UniformLocation>,
    bias: Option<glow::UniformLocation>,
    color: Option<glow::UniformLocation>,
}
impl FilterProgram {
    fn get_or_compile(gpu: &TextureContext) -> Result<Rc<Self>> {
        if let Some(cached) = gpu.filter_program.borrow().as_ref() {
            return Ok(cached.clone());
        }
        let _current = gpu.context.enter()?;
        let gl = &gpu.gl;
        // SAFETY: validated static sources, compiled on the owning worker context.
        let compiled = unsafe {
            let program = gl.create_program()?;
            let compile = || -> Result<()> {
                for (kind, source) in [
                    (glow::VERTEX_SHADER, VERTEX),
                    (glow::FRAGMENT_SHADER, FRAGMENT),
                ] {
                    let shader = gl.create_shader(kind)?;
                    gl.shader_source(shader, source);
                    gl.compile_shader(shader);
                    if !gl.get_shader_compile_status(shader) {
                        let error = gl.get_shader_info_log(shader);
                        gl.delete_shader(shader);
                        return Err(Error::Shader(error));
                    }
                    gl.attach_shader(program, shader);
                    gl.delete_shader(shader);
                }
                gl.link_program(program);
                if !gl.get_program_link_status(program) {
                    return Err(Error::Shader(gl.get_program_info_log(program)));
                }
                Ok(())
            };
            if let Err(error) = compile() {
                gl.delete_program(program);
                return Err(error);
            }
            let loc = |name| gl.get_uniform_location(program, name);
            Rc::new(Self {
                context: gpu.context.clone(),
                gl: gpu.gl.clone(),
                program,
                warmed: Cell::new(false),
                source: loc("source"),
                shadow: loc("shadow"),
                source_size: loc("source_size"),
                source_scale: loc("source_scale"),
                shadow_size: loc("shadow_size"),
                offset: loc("offset"),
                shadow_offset: loc("shadow_offset"),
                direction: loc("direction"),
                sigma: loc("sigma"),
                mode: loc("mode"),
                matrix: loc("matrix"),
                bias: loc("bias"),
                color: loc("color"),
            })
        };
        *gpu.filter_program.borrow_mut() = Some(compiled.clone());
        Ok(compiled)
    }

    fn draw_pass(
        &self,
        framebuffer: glow::Framebuffer,
        size: (u32, u32),
        source: Picture,
        pass: &Pass,
        flush: bool,
    ) -> Result<()> {
        let gl = &self.gl;
        // SAFETY: caller has entered `self.context`; inputs and distinct FBO belong
        // to this worker's context and uniforms are copied before release.
        unsafe {
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(framebuffer));
            gl.viewport(0, 0, size.0 as i32, size.1 as i32);
            gl.disable(glow::BLEND);
            gl.disable(glow::SCISSOR_TEST);
            gl.disable(glow::DEPTH_TEST);
            gl.use_program(Some(self.program));
            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(glow::TEXTURE_2D, Some(source.0));
            gl.uniform_1_i32(self.source.as_ref(), 0);
            gl.uniform_2_f32(self.source_size.as_ref(), source.1 as f32, source.2 as f32);
            gl.uniform_2_f32_slice(self.source_scale.as_ref(), &pass.source_scale);
            gl.uniform_2_f32_slice(self.offset.as_ref(), &pass.offset);
            gl.uniform_2_f32_slice(self.direction.as_ref(), &pass.direction);
            gl.uniform_1_f32(self.sigma.as_ref(), pass.sigma);
            gl.uniform_1_i32(self.mode.as_ref(), pass.mode);
            let mut matrix = [0.0; 16];
            let mut bias = [0.0; 4];
            for row in 0..4 {
                for column in 0..4 {
                    matrix[column * 4 + row] = pass.matrix[row * 5 + column];
                }
                bias[row] = pass.matrix[row * 5 + 4];
            }
            gl.uniform_matrix_4_f32_slice(self.matrix.as_ref(), false, &matrix);
            gl.uniform_4_f32_slice(self.bias.as_ref(), &bias);
            let (shadow, offset, color) = pass.shadow.unwrap_or((source, [0.0; 2], [0.0; 4]));
            gl.active_texture(glow::TEXTURE1);
            gl.bind_texture(glow::TEXTURE_2D, Some(shadow.0));
            gl.uniform_1_i32(self.shadow.as_ref(), 1);
            gl.uniform_2_f32(self.shadow_size.as_ref(), shadow.1 as f32, shadow.2 as f32);
            gl.uniform_2_f32_slice(self.shadow_offset.as_ref(), &offset);
            gl.uniform_4_f32_slice(self.color.as_ref(), &color);
            gl.draw_arrays(glow::TRIANGLES, 0, 3);
            if flush {
                gl.flush();
            }
            let error = gl.get_error();
            if error != glow::NO_ERROR {
                return Err(Error::Draw(error));
            }
        }
        self.warmed.set(true);
        Ok(())
    }
}
impl Drop for FilterProgram {
    fn drop(&mut self) {
        if let Ok(_current) = self.context.enter() {
            // SAFETY: deletion occurs on the creating worker and current context.
            unsafe {
                self.gl.delete_program(self.program);
            }
        }
    }
}

pub(in crate::render::composition) fn precompile(gpu: &TextureContext) -> Result<()> {
    let program = FilterProgram::get_or_compile(gpu)?;
    if program.warmed.get() {
        return Ok(());
    }
    let _current = gpu.context.enter()?;
    let empty = gpu.empty_picture();
    let mut pass = Pass::new((1, 1));
    pass.mode = 4;
    pass.shadow = Some((empty, [0.0; 2], [0.0; 4]));
    program.draw_pass(gpu.warmup_fbo(), (1, 1), empty, &pass, true)
}

pub(in crate::render::composition) struct Filters {
    gpu: TextureContext,
    targets: Vec<Target>,
    next: usize,
}
impl Filters {
    pub(in crate::render::composition) fn new(gpu: &TextureContext) -> Self {
        Self {
            gpu: gpu.clone(),
            targets: Vec::new(),
            next: 0,
        }
    }
    pub(in crate::render::composition) fn render(
        &mut self,
        mut picture: Picture,
        filters: &[Filter],
        mut scale: f32,
    ) -> Result<(Picture, f32, f32, f32)> {
        self.next = 0;
        let bounds =
            super::super::geometry::Bounds::new(picture.1 as f32 / scale, picture.2 as f32 / scale)
                .filters(filters)
                .0;
        let (_, _, bounded_scale) = super::super::geometry::target_size(
            bounds[2] - bounds[0],
            bounds[3] - bounds[1],
            scale,
        );
        // Reserve rounding room for the padding of each ordered pass.
        let active_count = crate::render::effects::active_filter_count(filters);
        let ratio = if bounded_scale < scale {
            bounded_scale / scale * (1.0 - (active_count as f32 * 4.0 + 2.0).min(1024.0) / 4096.0)
        } else {
            1.0
        };
        if ratio < 1.0 {
            let size = (
                (picture.1 as f32 * ratio).floor().max(1.0) as u32,
                (picture.2 as f32 * ratio).floor().max(1.0) as u32,
            );
            let mut pass = Pass::new(size);
            pass.mode = 3;
            pass.source_scale = [
                picture.1 as f32 / size.0 as f32,
                picture.2 as f32 / size.1 as f32,
            ];
            picture = self.draw(picture, pass)?;
            scale *= ratio;
        }
        let (mut x, mut y) = (0.0, 0.0);
        for filter in filters
            .iter()
            .filter(|filter| !crate::render::effects::is_noop_filter(filter))
        {
            match *filter {
                Filter::Blur(radius) => {
                    let sigma = super::super::geometry::blur_sigma(radius.0, scale);
                    picture = self.blur(picture, sigma)?;
                    let pad = (sigma * 3.0).ceil();
                    x -= pad;
                    y -= pad;
                }
                Filter::DropShadow {
                    offset_x,
                    offset_y,
                    blur,
                    color,
                } => {
                    let sigma = super::super::geometry::shadow_blur_sigma(blur.0, scale);
                    let blurred = self.blur(picture, sigma)?;
                    let pad = (sigma * 3.0).ceil();
                    let (dx, dy) = (offset_x.0 * scale, offset_y.0 * scale);
                    let left = (dx - pad).min(0.0).floor();
                    let top = (dy - pad).min(0.0).floor();
                    let right = (picture.1 as f32 + dx + pad).max(picture.1 as f32).ceil();
                    let bottom = (picture.2 as f32 + dy + pad).max(picture.2 as f32).ceil();
                    let mut pass = Pass::new(((right - left) as u32, (bottom - top) as u32));
                    pass.mode = 2;
                    pass.offset = [-left, -top];
                    let (r, g, b, a) = color.to_f64();
                    pass.shadow = Some((
                        blurred,
                        [dx - pad - left, dy - pad - top],
                        [r as f32, g as f32, b as f32, a as f32],
                    ));
                    picture = self.draw(picture, pass)?;
                    x += left;
                    y += top;
                }
                _ => {
                    if let Some(matrix) = filter.color_matrix() {
                        let mut pass = Pass::new((picture.1, picture.2));
                        pass.matrix = matrix;
                        picture = self.draw(picture, pass)?;
                    }
                }
            }
        }
        self.targets.truncate(self.next);
        Ok((picture, x, y, scale))
    }
    // Both textures cover the same full-screen viewport, in premultiplied RGBA.
    pub(in crate::render::composition) fn over(
        &mut self,
        front: Picture,
        back: Picture,
    ) -> Result<Picture> {
        self.next = 0;
        let mut pass = Pass::new((front.1, front.2));
        pass.mode = 4;
        pass.shadow = Some((back, [0.0; 2], [0.0; 4]));
        self.draw(front, pass)
    }

    fn blur(&mut self, mut picture: Picture, sigma: f32) -> Result<Picture> {
        if sigma <= 0.0 {
            return Ok(picture);
        }
        let pad = (sigma * 3.0).ceil() as u32;
        let size = (
            picture.1.saturating_add(pad.saturating_mul(2)),
            picture.2.saturating_add(pad.saturating_mul(2)),
        );
        if !sigma.is_finite() || size.0 > 4096 || size.1 > 4096 {
            return Err("filter dimensions exceed the pixel budget".into());
        }
        // Keep the separable kernel bounded for large radii. Downsample the
        // padded premultiplied picture, blur in that plane and expand it back;
        // this is a spatial filter, with no retained pixels between frames.
        if sigma > 8.0 {
            let factor = (sigma / 8.0).ceil();
            let small = (
                (size.0 as f32 / factor).ceil() as u32,
                (size.1 as f32 / factor).ceil() as u32,
            );
            let scale = [
                size.0 as f32 / small.0 as f32,
                size.1 as f32 / small.1 as f32,
            ];
            let mut pass = Pass::new(small);
            pass.mode = 3;
            pass.source_scale = scale;
            pass.offset = [pad as f32 / scale[0], pad as f32 / scale[1]];
            picture = self.draw(picture, pass)?;
            for (axis, scale) in scale.into_iter().enumerate() {
                let mut pass = Pass::new(small);
                pass.mode = 1;
                pass.sigma = sigma / scale;
                pass.direction[axis] = 1.0;
                picture = self.draw(picture, pass)?;
            }
            let mut pass = Pass::new(size);
            pass.mode = 3;
            pass.source_scale = [1.0 / scale[0], 1.0 / scale[1]];
            return self.draw(picture, pass);
        }
        for axis in 0..2 {
            let size = if axis == 0 {
                (picture.1.saturating_add(pad.saturating_mul(2)), picture.2)
            } else {
                (picture.1, picture.2.saturating_add(pad.saturating_mul(2)))
            };
            let mut pass = Pass::new(size);
            pass.mode = 1;
            pass.sigma = sigma;
            pass.offset[axis] = pad as f32;
            pass.direction[axis] = 1.0;
            picture = self.draw(picture, pass)?;
        }
        Ok(picture)
    }
    fn draw(&mut self, source: Picture, pass: Pass) -> Result<Picture> {
        let program = FilterProgram::get_or_compile(&self.gpu)?;
        let _current = self.gpu.context.enter()?;
        if self
            .targets
            .get(self.next)
            .is_none_or(|t| (t.width, t.height) != pass.size)
        {
            let target = Target::new(&self.gpu, pass.size.0, pass.size.1)?;
            if self.next == self.targets.len() {
                self.targets.push(target);
            } else {
                self.targets[self.next] = target;
            }
        }
        let target = &self.targets[self.next];
        self.next += 1;
        program.draw_pass(
            target.framebuffer,
            (target.width, target.height),
            source,
            &pass,
            false,
        )?;
        Ok(target.picture())
    }
}
