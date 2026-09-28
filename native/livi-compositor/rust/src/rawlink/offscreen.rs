//! the rawlink renderer tiers (PLAN.md N.10.1). each one composites the scene
//! into a persistent buffer through the output's damage tracker, then turns
//! only the damaged rects into dithered rgb565 inside the frame memfd.
//!
//! - gles565, gles on the render node, the dither pass draws into an rgb565
//!   renderbuffer and 565 is read back as is
//! - gles8888, the same pass into rgba8888 already on 565 levels, the cpu packs
//! - pixman, cpu composite into x8r8g8b8, one lut pass calibrates, dithers, packs

use std::ffi::CString;
use std::fs::File;

use smithay::backend::allocator::gbm::GbmDevice;
use smithay::backend::allocator::Fourcc;
use smithay::backend::egl::native::EGLSurfacelessDisplay;
use smithay::backend::egl::{EGLContext, EGLDisplay};
use smithay::backend::renderer::damage::OutputDamageTracker;
use smithay::backend::renderer::gles::{ffi, GlesRenderer, GlesTexture};
use smithay::backend::renderer::pixman::PixmanRenderer;
use smithay::backend::renderer::{Bind, Color32F, Offscreen as _, Texture as _};
use smithay::reexports::pixman::Image;

use super::dither::{self, Lut, Rect};
use crate::render::{collect_elements, Scene, CAL_GLSL};
use crate::state::CalState;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Tier {
    Gles565,
    Gles8888,
    Pixman,
}

impl Tier {
    pub fn name(self) -> &'static str {
        match self {
            Tier::Gles565 => "gles565",
            Tier::Gles8888 => "gles8888",
            Tier::Pixman => "pixman",
        }
    }
}

const PROBE_ORDER: [Tier; 3] = [Tier::Gles565, Tier::Gles8888, Tier::Pixman];

/// where the composite lives, and how it becomes 565.
enum Engine {
    Gles(Box<GlesEngine>),
    Pixman(Box<PixmanEngine>),
}

pub struct Offscreen {
    pub tier: Tier,
    engine: Engine,
    size: (i32, i32),
    tracker: OutputDamageTracker,
    // the composite buffer holds last frame's pixels, except right after a reset
    composite_valid: bool,
}

/// picks the tier from LIVI_RENDERER (auto probes in order), logs the choice
/// and why each earlier tier was passed over. a forced tier that fails is an error.
pub fn select(size: (i32, i32)) -> Result<Offscreen, String> {
    let want = std::env::var("LIVI_RENDERER").unwrap_or_default();
    let forced = match want.as_str() {
        "" | "auto" => None,
        other => Some(
            PROBE_ORDER
                .into_iter()
                .find(|t| t.name() == other)
                .ok_or_else(|| format!("LIVI_RENDERER={other} is not auto, gles565, gles8888 or pixman"))?,
        ),
    };
    let order: Vec<Tier> = match forced {
        Some(t) => vec![t],
        None => PROBE_ORDER.to_vec(),
    };

    let mut skipped: Vec<String> = Vec::new();
    // one egl setup serves both gles probes, the second only swaps the target
    let mut gles: Option<Result<Box<GlesSetup>, String>> = None;
    for tier in order {
        let engine = match tier {
            Tier::Pixman => PixmanEngine::new(size).map(|e| Engine::Pixman(Box::new(e))),
            Tier::Gles565 | Tier::Gles8888 => {
                match gles.take().unwrap_or_else(gles_setup) {
                    Ok(setup) => match GlesEngine::new(setup, tier == Tier::Gles565, size) {
                        Ok(engine) => Ok(Engine::Gles(Box::new(engine))),
                        Err((setup, e)) => {
                            gles = Some(Ok(setup));
                            Err(e)
                        }
                    },
                    Err(e) => {
                        gles = Some(Err(e.clone()));
                        Err(e)
                    }
                }
            }
        };
        match engine {
            Ok(engine) => {
                let why = if skipped.is_empty() {
                    String::new()
                } else {
                    format!(" ({})", skipped.join("; "))
                };
                let node = match &engine {
                    Engine::Gles(g) => format!(" on {}", g.setup.node),
                    Engine::Pixman(_) => String::new(),
                };
                log::info!("renderer {}{node}{why}", tier.name());
                return Ok(Offscreen {
                    tier,
                    engine,
                    size,
                    tracker: OutputDamageTracker::new(size, 1.0, smithay::utils::Transform::Normal),
                    composite_valid: false,
                });
            }
            Err(e) => skipped.push(format!("{} skipped, {e}", tier.name())),
        }
    }
    Err(format!("renderer {want} failed its probe ({})", skipped.join("; ")))
}

impl Offscreen {
    /// whether clients may be offered linux-dmabuf.
    pub fn dmabuf(&self) -> bool {
        matches!(&self.engine, Engine::Gles(g) if g.setup.dmabuf)
    }

    pub fn gles_renderer(&mut self) -> Option<&mut GlesRenderer> {
        match &mut self.engine {
            Engine::Gles(g) => Some(&mut g.setup.renderer),
            Engine::Pixman(_) => None,
        }
    }

    /// the next render redraws and converts everything, for changes the
    /// tracker can't see (calibration, backdrop).
    pub fn reset(&mut self) {
        self.tracker = OutputDamageTracker::new(self.size, 1.0, smithay::utils::Transform::Normal);
        self.composite_valid = false;
    }

    /// composites the scene and writes the damaged rects into `frame` as 565.
    /// None when nothing changed. `full` converts the whole screen regardless.
    pub fn render(
        &mut self,
        scene: &Scene,
        clear: Color32F,
        cal: &CalState,
        full: bool,
        frame: &mut [u8],
        stride: usize,
    ) -> Result<Option<Vec<Rect>>, String> {
        let age = if self.composite_valid { 1 } else { 0 };
        let (w, h) = self.size;
        let damage = match &mut self.engine {
            Engine::Gles(g) => {
                let renderer = &mut g.setup.renderer;
                let elements = collect_elements(renderer, scene, 0, None);
                let mut fb = renderer.bind(&mut g.gl.composite).map_err(|e| e.to_string())?;
                let res = self
                    .tracker
                    .render_output(renderer, &mut fb, age, &elements, clear)
                    .map_err(|e| format!("{e:?}"))?;
                res.damage.cloned()
            }
            Engine::Pixman(p) => {
                let renderer = &mut p.renderer;
                let elements = collect_elements(renderer, scene, 0, None);
                let mut fb = renderer.bind(&mut p.image).map_err(|e| e.to_string())?;
                let res = self
                    .tracker
                    .render_output(renderer, &mut fb, age, &elements, clear)
                    .map_err(|e| format!("{e:?}"))?;
                res.damage.cloned()
            }
        };
        self.composite_valid = true;

        let rects: Vec<Rect> = if full {
            vec![Rect { x: 0, y: 0, w: w as usize, h: h as usize }]
        } else {
            let Some(damage) = damage else { return Ok(None) };
            damage.iter().filter_map(|r| clip(r, self.size)).collect()
        };
        if rects.is_empty() {
            return Ok(None);
        }
        match &mut self.engine {
            Engine::Gles(g) => g.convert(cal, &rects, frame, stride)?,
            Engine::Pixman(p) => p.convert(cal, &rects, frame, stride),
        }
        Ok(Some(rects))
    }
}

fn clip(r: &smithay::utils::Rectangle<i32, smithay::utils::Physical>, size: (i32, i32)) -> Option<Rect> {
    let x0 = r.loc.x.clamp(0, size.0);
    let y0 = r.loc.y.clamp(0, size.1);
    let x1 = (r.loc.x + r.size.w).clamp(0, size.0);
    let y1 = (r.loc.y + r.size.h).clamp(0, size.1);
    (x1 > x0 && y1 > y0).then(|| Rect {
        x: x0 as usize,
        y: y0 as usize,
        w: (x1 - x0) as usize,
        h: (y1 - y0) as usize,
    })
}

struct GlesSetup {
    renderer: GlesRenderer,
    node: String,
    dmabuf: bool,
}

fn render_node() -> Option<String> {
    if let Ok(p) = std::env::var("LIVI_RENDER_NODE")
        && !p.is_empty()
    {
        return Some(p);
    }
    let mut nodes: Vec<String> = std::fs::read_dir("/dev/dri")
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path().to_string_lossy().into_owned())
        .filter(|p| p.rsplit('/').next().is_some_and(|n| n.starts_with("renderD")))
        .collect();
    nodes.sort();
    nodes.into_iter().next()
}

/// egl through gbm on the render node, or mesa's surfaceless platform when the
/// machine has no node at all (ci on llvmpipe).
fn gles_setup() -> Result<Box<GlesSetup>, String> {
    let (display, node) = match render_node() {
        Some(node) => {
            let file = File::options()
                .read(true)
                .write(true)
                .open(&node)
                .map_err(|e| format!("can't open {node}, {e}"))?;
            let gbm = GbmDevice::new(file).map_err(|e| format!("no gbm on {node}, {e}"))?;
            // the display keeps the gbm device alive for as long as it lives
            let display = unsafe { EGLDisplay::new(gbm) }
                .map_err(|e| format!("no egl on {node}, {e}"))?;
            (display, node)
        }
        None => {
            let display = unsafe { EGLDisplay::new(EGLSurfacelessDisplay) }
                .map_err(|e| format!("no render node and no surfaceless egl, {e}"))?;
            (display, "surfaceless egl".to_string())
        }
    };
    let dmabuf = display.extensions().iter().any(|e| e == "EGL_EXT_image_dma_buf_import");
    let context = EGLContext::new(&display).map_err(|e| format!("no egl context, {e}"))?;
    let renderer = unsafe { GlesRenderer::new(context) }.map_err(|e| format!("no gles 2 renderer, {e}"))?;
    Ok(Box::new(GlesSetup { renderer, node, dmabuf }))
}

struct GlesEngine {
    setup: Box<GlesSetup>,
    gl: GlesParts,
}

/// a gles engine short of its setup, what the target probe builds.
struct GlesParts {
    composite: GlesTexture,
    fbo: u32,
    // readback format and type, rgb/565 on tier 1 and rgba/ubyte on tier 2
    read: (u32, u32),
    program: u32,
    a_pos: u32,
    u_tex: i32,
    u_gamma: i32,
    u_contrast: i32,
    u_gain: i32,
    vbo: u32,
    scratch: Vec<u8>,
}

const VERT: &str = r#"
#version 100
attribute vec2 a_pos;
varying vec2 v_coords;
void main() {
    v_coords = a_pos;
    gl_Position = vec4(a_pos * 2.0 - 1.0, 0.0, 1.0);
}
"#;

const FRAG_HEAD: &str = r#"
#version 100
#ifdef GL_FRAGMENT_PRECISION_HIGH
precision highp float;
#else
precision mediump float;
#endif
uniform sampler2D tex;
varying vec2 v_coords;
"#;

// gl y runs up from row 0 and so does the readback, so screen row y is gl row y
// and gl_FragCoord is already the screen coordinate the dither anchors to
const FRAG_MAIN: &str = r#"
void main() {
    vec3 c = calibrate(texture2D(tex, v_coords).rgb);
    gl_FragColor = vec4(dither565(c, gl_FragCoord.xy), 1.0);
}
"#;

unsafe fn compile(gl: &ffi::Gles2, kind: u32, src: &str) -> Result<u32, String> {
    unsafe {
        let shader = gl.CreateShader(kind);
        let c = CString::new(src).unwrap();
        gl.ShaderSource(shader, 1, &c.as_ptr(), std::ptr::null());
        gl.CompileShader(shader);
        let mut ok = 0;
        gl.GetShaderiv(shader, ffi::COMPILE_STATUS, &mut ok);
        if ok == 0 {
            let mut log = vec![0u8; 1024];
            let mut len = 0;
            gl.GetShaderInfoLog(shader, log.len() as i32, &mut len, log.as_mut_ptr() as *mut _);
            gl.DeleteShader(shader);
            return Err(format!("dither shader failed, {}", String::from_utf8_lossy(&log[..len.max(0) as usize])));
        }
        Ok(shader)
    }
}

impl GlesEngine {
    /// the probe for either gles tier, handing the setup back when it fails so
    /// the next tier can reuse it.
    fn new(mut setup: Box<GlesSetup>, rgb565: bool, size: (i32, i32)) -> Result<GlesEngine, (Box<GlesSetup>, String)> {
        match Self::build(&mut setup.renderer, rgb565, size) {
            Ok(gl) => Ok(GlesEngine { setup, gl }),
            Err(e) => Err((setup, e)),
        }
    }

    fn build(renderer: &mut GlesRenderer, rgb565: bool, size: (i32, i32)) -> Result<GlesParts, String> {
        let composite: GlesTexture = renderer
            .create_buffer(Fourcc::Abgr8888, smithay::utils::Size::from(size))
            .map_err(|e| format!("no rgba8888 texture, {e}"))?;
        let frag = format!("{FRAG_HEAD}{CAL_GLSL}{}{FRAG_MAIN}", dither::glsl());
        let res = renderer
            .with_context(|gl| unsafe {
                let mut fbo = 0;
                gl.GenFramebuffers(1, &mut fbo);
                gl.BindFramebuffer(ffi::FRAMEBUFFER, fbo);
                if rgb565 {
                    let mut rb = 0;
                    gl.GenRenderbuffers(1, &mut rb);
                    gl.BindRenderbuffer(ffi::RENDERBUFFER, rb);
                    gl.RenderbufferStorage(ffi::RENDERBUFFER, ffi::RGB565, size.0, size.1);
                    gl.FramebufferRenderbuffer(ffi::FRAMEBUFFER, ffi::COLOR_ATTACHMENT0, ffi::RENDERBUFFER, rb);
                    gl.BindRenderbuffer(ffi::RENDERBUFFER, 0);
                } else {
                    let mut tex = 0;
                    gl.GenTextures(1, &mut tex);
                    gl.BindTexture(ffi::TEXTURE_2D, tex);
                    gl.TexImage2D(
                        ffi::TEXTURE_2D, 0, ffi::RGBA as i32, size.0, size.1, 0,
                        ffi::RGBA, ffi::UNSIGNED_BYTE, std::ptr::null(),
                    );
                    gl.FramebufferTexture2D(ffi::FRAMEBUFFER, ffi::COLOR_ATTACHMENT0, ffi::TEXTURE_2D, tex, 0);
                    gl.BindTexture(ffi::TEXTURE_2D, 0);
                }
                let status = gl.CheckFramebufferStatus(ffi::FRAMEBUFFER);
                let mut fmt = 0;
                let mut ty = 0;
                gl.GetIntegerv(ffi::IMPLEMENTATION_COLOR_READ_FORMAT, &mut fmt);
                gl.GetIntegerv(ffi::IMPLEMENTATION_COLOR_READ_TYPE, &mut ty);
                gl.BindFramebuffer(ffi::FRAMEBUFFER, 0);
                if status != ffi::FRAMEBUFFER_COMPLETE {
                    return Err(format!(
                        "{} framebuffer incomplete (0x{status:x})",
                        if rgb565 { "rgb565" } else { "rgba8888" }
                    ));
                }
                if rgb565 && (fmt as u32, ty as u32) != (ffi::RGB, ffi::UNSIGNED_SHORT_5_6_5) {
                    return Err(format!("565 read format is 0x{fmt:x}/0x{ty:x}, not rgb/5_6_5"));
                }

                let vs = compile(gl, ffi::VERTEX_SHADER, VERT)?;
                let fs = compile(gl, ffi::FRAGMENT_SHADER, &frag)?;
                let program = gl.CreateProgram();
                gl.AttachShader(program, vs);
                gl.AttachShader(program, fs);
                gl.LinkProgram(program);
                gl.DeleteShader(vs);
                gl.DeleteShader(fs);
                let mut ok = 0;
                gl.GetProgramiv(program, ffi::LINK_STATUS, &mut ok);
                if ok == 0 {
                    return Err("dither program failed to link".to_string());
                }
                let loc = |n: &str| {
                    let c = CString::new(n).unwrap();
                    gl.GetUniformLocation(program, c.as_ptr())
                };
                let a_pos = {
                    let c = CString::new("a_pos").unwrap();
                    gl.GetAttribLocation(program, c.as_ptr()) as u32
                };
                let quad: [f32; 8] = [0.0, 0.0, 1.0, 0.0, 0.0, 1.0, 1.0, 1.0];
                let mut vbo = 0;
                gl.GenBuffers(1, &mut vbo);
                gl.BindBuffer(ffi::ARRAY_BUFFER, vbo);
                gl.BufferData(
                    ffi::ARRAY_BUFFER,
                    std::mem::size_of_val(&quad) as isize,
                    quad.as_ptr() as *const _,
                    ffi::STATIC_DRAW,
                );
                gl.BindBuffer(ffi::ARRAY_BUFFER, 0);
                Ok((fbo, program, a_pos, loc("tex"), loc("u_gamma"), loc("u_contrast"), loc("u_gain"), vbo))
            })
            .map_err(|e| format!("gl context lost, {e}"))??;
        let (fbo, program, a_pos, u_tex, u_gamma, u_contrast, u_gain, vbo) = res;
        let read = if rgb565 {
            (ffi::RGB, ffi::UNSIGNED_SHORT_5_6_5)
        } else {
            (ffi::RGBA, ffi::UNSIGNED_BYTE)
        };
        let bpp = if rgb565 { 2 } else { 4 };
        Ok(GlesParts {
            composite,
            fbo,
            read,
            program,
            a_pos,
            u_tex,
            u_gamma,
            u_contrast,
            u_gain,
            vbo,
            scratch: vec![0; (size.0 * size.1) as usize * bpp],
        })
    }

    /// the dither pass over `rects` from the composite into our target, then
    /// the readback of just those rects.
    fn convert(&mut self, cal: &CalState, rects: &[Rect], frame: &mut [u8], stride: usize) -> Result<(), String> {
        let GlesEngine { setup, gl: p } = self;
        let tex = p.composite.tex_id();
        let (w, h) = (p.composite.width() as i32, p.composite.height() as i32);
        let GlesParts { fbo, read, program, a_pos, u_tex, u_gamma, u_contrast, u_gain, vbo, .. } = *p;
        let scratch = &mut p.scratch;
        setup
            .renderer
            .with_context(|gl| unsafe {
                gl.BindFramebuffer(ffi::FRAMEBUFFER, fbo);
                gl.Viewport(0, 0, w, h);
                gl.Disable(ffi::BLEND);
                gl.Enable(ffi::SCISSOR_TEST);
                gl.UseProgram(program);
                gl.ActiveTexture(ffi::TEXTURE0);
                gl.BindTexture(ffi::TEXTURE_2D, tex);
                gl.TexParameteri(ffi::TEXTURE_2D, ffi::TEXTURE_MIN_FILTER, ffi::NEAREST as i32);
                gl.TexParameteri(ffi::TEXTURE_2D, ffi::TEXTURE_MAG_FILTER, ffi::NEAREST as i32);
                gl.Uniform1i(u_tex, 0);
                gl.Uniform1f(u_gamma, cal.gamma);
                gl.Uniform1f(u_contrast, cal.contrast);
                gl.Uniform3f(u_gain, cal.gain[0], cal.gain[1], cal.gain[2]);
                gl.BindBuffer(ffi::ARRAY_BUFFER, vbo);
                gl.EnableVertexAttribArray(a_pos);
                gl.VertexAttribPointer(a_pos, 2, ffi::FLOAT, ffi::FALSE, 0, std::ptr::null());
                for r in rects {
                    gl.Scissor(r.x as i32, r.y as i32, r.w as i32, r.h as i32);
                    gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);
                }
                gl.PixelStorei(ffi::PACK_ALIGNMENT, 1);
                for r in rects {
                    gl.ReadPixels(
                        r.x as i32, r.y as i32, r.w as i32, r.h as i32,
                        read.0, read.1, scratch.as_mut_ptr() as *mut _,
                    );
                    if read.1 == ffi::UNSIGNED_SHORT_5_6_5 {
                        dither::copy_565(scratch, frame, stride, *r);
                    } else {
                        dither::pack_rgba(scratch, frame, stride, *r);
                    }
                }
                // hand smithay back the state it expects to find
                gl.PixelStorei(ffi::PACK_ALIGNMENT, 4);
                gl.DisableVertexAttribArray(a_pos);
                gl.BindBuffer(ffi::ARRAY_BUFFER, 0);
                gl.BindTexture(ffi::TEXTURE_2D, 0);
                gl.UseProgram(0);
                gl.Disable(ffi::SCISSOR_TEST);
                gl.BindFramebuffer(ffi::FRAMEBUFFER, 0);
                gl.GetError()
            })
            .map_err(|e| format!("gl context lost, {e}"))
            .and_then(|err| if err == ffi::NO_ERROR { Ok(()) } else { Err(format!("gl error 0x{err:x} in the dither pass")) })
    }
}

struct PixmanEngine {
    renderer: PixmanRenderer,
    image: Image<'static, 'static>,
    lut: Lut,
}

impl PixmanEngine {
    fn new(size: (i32, i32)) -> Result<Self, String> {
        let mut renderer = PixmanRenderer::new().map_err(|e| format!("no pixman, {e}"))?;
        let image = renderer
            .create_buffer(Fourcc::Xrgb8888, smithay::utils::Size::from(size))
            .map_err(|e| format!("no x8r8g8b8 image, {e}"))?;
        let lut = Lut::new(&CalState { active: false, gamma: 1.0, contrast: 1.0, gain: [1.0; 3] });
        Ok(Self { renderer, image, lut })
    }

    fn convert(&mut self, cal: &CalState, rects: &[Rect], frame: &mut [u8], stride: usize) {
        if !self.lut.matches(cal) {
            self.lut = Lut::new(cal);
        }
        let px_stride = self.image.stride() / 4;
        let len = px_stride * self.image.height();
        // the image stays alive and unbound for the whole pass
        let src = unsafe { std::slice::from_raw_parts(self.image.data() as *const u32, len) };
        for r in rects {
            dither::pack_xrgb(&self.lut, src, px_stride, frame, stride, *r);
        }
    }
}
