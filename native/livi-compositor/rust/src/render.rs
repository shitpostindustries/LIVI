//! Per-screen rendering: backdrop, tagged video planes, the UI plane,
//! compositor decorations and dialogs, with the optional full-output
//! calibration (gamma/contrast/gain) shader pass.

use smithay::backend::renderer::damage::OutputDamageTracker;
use smithay::backend::renderer::element::memory::{MemoryRenderBuffer, MemoryRenderBufferRenderElement};
use smithay::backend::renderer::element::surface::{
    render_elements_from_surface_tree, WaylandSurfaceRenderElement,
};
use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::Kind as ElementKind;
use smithay::backend::renderer::gles::{
    GlesRenderer, GlesTexProgram, GlesTexture, Uniform, UniformName, UniformType,
};
use smithay::backend::renderer::{Bind, Color32F, Frame, ImportAll, ImportMem, Offscreen, Renderer};
use smithay::utils::{Logical, Point, Rectangle, Transform};

use crate::deco::DecoSet;
use crate::state::{Kind, LiviState, Screen, TopLevel, BTN_GAP, BTN_W};

smithay::backend::renderer::element::render_elements! {
    pub LiviElement<R> where R: ImportAll + ImportMem;
    Surface=WaylandSurfaceRenderElement<R>,
    Deco=MemoryRenderBufferRenderElement<R>,
}

// The calibration fragment shader, applied over the composited frame.
// Follows smithay's custom-texture-shader contract: the //_DEFINES_ line,
// the v_coords varying and the EXTERNAL sampler variant are mandatory.
const CAL_FRAG: &str = r#"
#version 100
//_DEFINES_
#if defined(EXTERNAL)
#extension GL_OES_EGL_image_external : require
#endif
precision mediump float;
#if defined(EXTERNAL)
uniform samplerExternalOES tex;
#else
uniform sampler2D tex;
#endif
uniform float alpha;
varying vec2 v_coords;
uniform float u_gamma;
uniform float u_contrast;
uniform vec3 u_gain;
#if defined(DEBUG_FLAGS)
uniform float tint;
#endif
void main() {
    vec3 c = texture2D(tex, v_coords).rgb;
    c = pow(c, vec3(1.0 / u_gamma));
    c = (c - 0.5) * u_contrast + 0.5;
    c = clamp(c * u_gain, 0.0, 1.0);
    gl_FragColor = vec4(c, 1.0) * alpha;
}
"#;

pub fn cal_program(state: &mut LiviState) -> Option<GlesTexProgram> {
    if let Some(p) = state.host.cal_program.clone() {
        return Some(p);
    }
    let renderer = state.host.renderer.as_mut()?;
    match renderer.compile_custom_texture_shader(
        CAL_FRAG,
        &[
            UniformName::new("u_gamma", UniformType::_1f),
            UniformName::new("u_contrast", UniformType::_1f),
            UniformName::new("u_gain", UniformType::_3f),
        ],
    ) {
        Ok(p) => {
            state.host.cal_program = Some(p.clone());
            Some(p)
        }
        Err(e) => {
            log::error!("cal shader compile failed: {e}");
            state.cal.active = false;
            None
        }
    }
}

/// Committed size of a toplevel's main surface.
pub fn surface_size(surface: &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface) -> (i32, i32) {
    smithay::backend::renderer::utils::with_renderer_surface_state(surface, |s| {
        s.surface_size().map(|sz| (sz.w, sz.h)).unwrap_or((0, 0))
    })
    .unwrap_or((0, 0))
}

/// Topmost sub-surface of `root` under a root-local point.
pub fn surface_under(
    root: &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
    local: Point<f64, Logical>,
) -> Option<(
    smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
    Point<f64, Logical>,
)> {
    use smithay::wayland::compositor::{with_surface_tree_downward, TraversalAction};
    let found: std::cell::RefCell<Option<(_, Point<f64, Logical>)>> = std::cell::RefCell::new(None);
    with_surface_tree_downward(
        root,
        Point::<i32, Logical>::from((0, 0)),
        |_, states, offset| {
            let mut off = *offset;
            off += states
                .cached_state
                .get::<smithay::wayland::compositor::SubsurfaceCachedState>()
                .current()
                .location;
            TraversalAction::DoChildren(off)
        },
        |surface, states, offset| {
            let mut off = *offset;
            off += states
                .cached_state
                .get::<smithay::wayland::compositor::SubsurfaceCachedState>()
                .current()
                .location;
            // the traversal already holds the states lock and a with_states
            // re-entry here deadlocks, so read the state off the data_map
            let size = states
                .data_map
                .get::<smithay::backend::renderer::utils::RendererSurfaceStateUserData>()
                .and_then(|d| d.lock().unwrap().surface_size());
            if let Some(size) = size {
                let rect = Rectangle::<f64, Logical>::new(
                    (off.x as f64, off.y as f64).into(),
                    (size.w as f64, size.h as f64).into(),
                );
                if rect.contains(local) {
                    *found.borrow_mut() = Some((
                        surface.clone(),
                        local - Point::from((off.x as f64, off.y as f64)),
                    ));
                }
            }
        },
        |_, _, _| true,
    );
    found.into_inner()
}

/// The parts of the compositor state a scene is built from, borrowed apart
/// from the backend that owns the renderer.
pub struct Scene<'a> {
    pub screens: &'a [Screen],
    pub toplevels: &'a [TopLevel],
    pub video_order: &'a [usize],
}

impl<'a> Scene<'a> {
    pub fn new(screens: &'a [Screen], toplevels: &'a [TopLevel], video_order: &'a [usize]) -> Self {
        Self { screens, toplevels, video_order }
    }
}

fn surface_elements<R>(
    renderer: &mut R,
    t: &TopLevel,
    sx: i32,
) -> impl Iterator<Item = LiviElement<R>>
where
    R: Renderer + ImportAll + ImportMem,
    R::TextureId: Send + Clone + 'static,
{
    // screen-local offset, the output renders layout range [sx .. sx+sw]
    let loc = Point::<i32, smithay::utils::Physical>::from((t.position.x - sx, t.position.y));
    render_elements_from_surface_tree::<_, WaylandSurfaceRenderElement<R>>(
        renderer,
        t.toplevel.wl_surface(),
        loc,
        smithay::utils::Scale::from(1.0),
        1.0,
        ElementKind::Unspecified,
    )
    .into_iter()
    .map(LiviElement::Surface)
}

/// Collect the render elements for one screen, top to bottom (renderer order).
pub fn collect_elements<R>(
    renderer: &mut R,
    scene: &Scene,
    screen_idx: usize,
    deco: Option<&DecoSet>,
) -> Vec<LiviElement<R>>
where
    R: Renderer + ImportAll + ImportMem,
    R::TextureId: Send + Clone + 'static,
{
    let s = &scene.screens[screen_idx];
    let (sx, sw) = (s.x, s.width);
    let mut elements: Vec<LiviElement<R>> = Vec::new();

    // dialogs (top)
    for t in scene.toplevels.iter().filter(|t| t.kind == Kind::Dialog && t.screen_idx == screen_idx) {
        elements.extend(surface_elements(renderer, t, sx));
    }

    // decorations
    if let Some(set) = deco {
        let slot = BTN_W + BTN_GAP;
        let items: [(&MemoryRenderBuffer, Point<i32, Logical>); 5] = [
            (&set.btn_close, Point::from((sw - slot, 0))),
            (&set.btn_fs, Point::from((sw - 2 * slot, 0))),
            (&set.btn_min, Point::from((sw - 3 * slot, 0))),
            (&set.title, Point::from((12, 0))),
            (&set.titlebar, Point::from((0, 0))),
        ];
        for (buf, pos) in items {
            if let Ok(el) = MemoryRenderBufferRenderElement::from_buffer(
                renderer,
                pos.to_f64().to_physical(1.0),
                buf,
                None,
                None,
                None,
                ElementKind::Unspecified,
            ) {
                elements.push(LiviElement::Deco(el));
            }
        }
    }

    // UI plane
    for t in scene.toplevels.iter().filter(|t| t.kind == Kind::Ui && t.screen_idx == screen_idx) {
        elements.extend(surface_elements(renderer, t, sx));
    }

    // video planes, top-to-bottom = reverse of the bottom-to-top order
    for &vi in scene.video_order.iter().rev() {
        let Some(t) = scene.toplevels.get(vi) else { continue };
        if t.kind != Kind::Video || t.screen_idx != screen_idx || !t.visible {
            continue;
        }
        elements.extend(surface_elements(renderer, t, sx));
    }

    elements
}

/// Rebuild a windowed screen's decoration set when its width changed.
fn ensure_deco(state: &mut LiviState, screen_idx: usize) {
    let s = &state.screens[screen_idx];
    if s.fullscreen {
        return;
    }
    let stale = state.host.deco.get(&screen_idx).map(|d| d.titlebar_w != s.width).unwrap_or(true);
    if stale {
        let set = crate::deco::build(&s.role, s.width);
        state.host.deco.insert(screen_idx, set);
    }
}

/// The clear colour behind everything on a screen.
pub fn backdrop_color(s: &Screen) -> Color32F {
    let c = if std::env::var("LIVI_DEBUG_BG").is_ok() {
        [0.55, 0.0, 0.55, 1.0]
    } else if s.has_backdrop_color {
        s.backdrop_color
    } else {
        [0.0, 0.0, 0.0, 1.0]
    };
    Color32F::new(c[0], c[1], c[2], c[3])
}

pub fn cal_uniforms(cal: &crate::state::CalState) -> Vec<Uniform<'static>> {
    vec![
        Uniform::new("u_gamma", cal.gamma),
        Uniform::new("u_contrast", cal.contrast),
        Uniform::new("u_gain", (cal.gain[0], cal.gain[1], cal.gain[2])),
    ]
}

/// A host window's damage state, kept across frames so only what changed is
/// redrawn. Dropped (and rebuilt at full damage) on resize or a mode change.
pub struct WindowDamage {
    tracker: OutputDamageTracker,
    size: (i32, i32),
    calibrated: bool,
    // the composite the calibration pass reads, persistent so it can take partial damage
    offscreen: Option<GlesTexture>,
}

fn window_damage(slot: &mut Option<WindowDamage>, size: (i32, i32), calibrated: bool) -> &mut WindowDamage {
    if slot.as_ref().is_some_and(|d| d.size != size || d.calibrated != calibrated) {
        *slot = None;
    }
    slot.get_or_insert_with(|| WindowDamage {
        // gl window surfaces have a bottom-left origin, so the on-screen pass
        // renders Flipped180 and the offscreen composite stays Normal
        tracker: OutputDamageTracker::new(
            size,
            1.0,
            if calibrated { Transform::Normal } else { Transform::Flipped180 },
        ),
        size,
        calibrated,
        offscreen: None,
    })
}

pub fn render_screen(state: &mut LiviState, screen_idx: usize) {
    if state.host.renderer.is_none() {
        return;
    }
    let Some(w) = state.host.window_for_screen(screen_idx) else {
        return;
    };
    if !w.configured {
        return;
    }
    let (width, height) = (w.width, w.height);
    if width <= 0 || height <= 0 {
        return;
    }
    w.needs_redraw = false;

    let clear = backdrop_color(&state.screens[screen_idx]);
    ensure_deco(state, screen_idx);
    let cal = if state.cal.active { cal_program(state) } else { None };
    let uniforms = cal_uniforms(&state.cal);

    let res = {
        let LiviState { host, screens, toplevels, video_order, .. } = state;
        let scene = Scene::new(screens, toplevels, video_order);
        let crate::host::HostState { renderer, windows, deco, .. } = host;
        let renderer = renderer.as_mut().unwrap();
        let deco = if screens[screen_idx].fullscreen { None } else { deco.get(&screen_idx) };
        let elements = collect_elements(renderer, &scene, screen_idx, deco);
        let hw = windows
            .iter_mut()
            .find(|(i, _)| *i == screen_idx)
            .map(|(_, w)| w)
            .unwrap();
        let damage = window_damage(&mut hw.damage, (width, height), cal.is_some());
        if let Some(program) = cal.as_ref() {
            render_calibrated(renderer, &mut hw.egl_surface, damage, &elements, clear, program, &uniforms)
        } else {
            render_direct(renderer, &mut hw.egl_surface, damage, &elements, clear)
        }
    };

    match res {
        Ok(true) => {
            crate::host::request_frame(state, screen_idx);
            if let Some(w) = state.host.window_for_screen(screen_idx)
                && let Err(e) = w.egl_surface.swap_buffers(None) {
                    log::error!("swap_buffers failed: {e}");
                }
            crate::host::send_frame_callbacks(state);
        }
        // nothing visible changed, the host keeps the last buffer and the
        // clients still get their callbacks so they don't stall
        Ok(false) => crate::host::send_frame_callbacks(state),
        Err(e) => log::error!("render failed: {e}"),
    }
}

type RenderResult = Result<bool, Box<dyn std::error::Error>>;

/// Composite straight onto the window surface, reusing what its buffer age
/// says is still there. Answers whether anything was drawn.
fn render_direct(
    renderer: &mut GlesRenderer,
    egl_surface: &mut smithay::backend::egl::EGLSurface,
    damage: &mut WindowDamage,
    elements: &[LiviElement<GlesRenderer>],
    clear: Color32F,
) -> RenderResult {
    // egl only knows the age of the current draw surface, asking before
    // that is BAD_SURFACE and a needless full redraw
    unsafe { renderer.egl_context().make_current_with_surface(egl_surface)? };
    let age = egl_surface.buffer_age().unwrap_or(0).max(0) as usize;
    let mut fb = renderer.bind(egl_surface)?;
    let res = damage.tracker.render_output(renderer, &mut fb, age, elements, clear)?;
    Ok(res.damage.is_some())
}

/// Composite `elements` into the persistent offscreen texture, then draw the
/// whole result through the calibration shader onto the window surface.
fn render_calibrated(
    renderer: &mut GlesRenderer,
    egl_surface: &mut smithay::backend::egl::EGLSurface,
    damage: &mut WindowDamage,
    elements: &[LiviElement<GlesRenderer>],
    clear: Color32F,
    program: &GlesTexProgram,
    uniforms: &[Uniform<'static>],
) -> RenderResult {
    let (w, h) = damage.size;
    // a fresh texture holds nothing, age 0 makes the tracker redraw it all
    let age = if damage.offscreen.is_some() { 1 } else { 0 };
    if damage.offscreen.is_none() {
        damage.offscreen =
            Some(renderer.create_buffer(Fourcc::Abgr8888, smithay::utils::Size::from((w, h)))?);
    }
    let tex = damage.offscreen.as_mut().unwrap();
    {
        let mut fb = renderer.bind(tex)?;
        let res = damage.tracker.render_output(renderer, &mut fb, age, elements, clear)?;
        if res.damage.is_none() {
            return Ok(false);
        }
    }
    // the window's buffers rotate, so the blit covers all of it, the flip happens here
    let mut fb = renderer.bind(egl_surface)?;
    let mut frame =
        renderer.render(&mut fb, smithay::utils::Size::from((w, h)), Transform::Flipped180)?;
    let full = Rectangle::<i32, smithay::utils::Physical>::new((0, 0).into(), (w, h).into());
    frame.render_texture_from_to(
        tex,
        Rectangle::<f64, smithay::utils::Buffer>::new(
            (0.0, 0.0).into(),
            (w as f64, h as f64).into(),
        ),
        full,
        &[full],
        &[],
        Transform::Normal,
        1.0,
        Some(program),
        uniforms,
    )?;
    let _ = frame.finish()?;
    Ok(true)
}
