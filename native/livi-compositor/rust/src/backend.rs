//! backend selection (LIVI_BACKEND=host|rawlink, host by default) and the
//! calls the rest of the compositor makes without caring which one runs.

use smithay::reexports::calloop::LoopHandle;

use crate::state::LiviState;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Backend {
    /// windows of an outer wayland session, for desktop development
    Host,
    /// the headless output rawlink carries to the head unit
    Rawlink,
}

pub fn from_env() -> Backend {
    match std::env::var("LIVI_BACKEND").as_deref() {
        Err(_) | Ok("") | Ok("host") => Backend::Host,
        Ok("rawlink") => Backend::Rawlink,
        Ok(other) => {
            log::error!("LIVI_BACKEND={other} is not host or rawlink");
            std::process::exit(1);
        }
    }
}

/// LIVI_OUTPUT_SIZE as `WxH`, when it's set and sane.
pub fn output_size_env() -> Option<(i32, i32)> {
    let v = std::env::var("LIVI_OUTPUT_SIZE").ok()?;
    let (w, h) = v.split_once('x')?;
    let (w, h): (i32, i32) = (w.parse().ok()?, h.parse().ok()?);
    (w > 0 && h > 0).then_some((w, h))
}

pub fn init(state: &mut LiviState, handle: &LoopHandle<'static, LiviState>) {
    match state.backend {
        Backend::Host => crate::host::init(state, handle),
        Backend::Rawlink => crate::rawlink::init(state, handle),
    }
}

/// something in the scene may have changed, the damage trackers find out what.
pub fn damage_all(state: &mut LiviState) {
    match state.backend {
        Backend::Host => crate::host::damage_all(state),
        Backend::Rawlink => crate::rawlink::damage(state),
    }
}

/// a change the trackers can't see (backdrop, calibration), redraw everything.
pub fn damage_full(state: &mut LiviState) {
    match state.backend {
        Backend::Host => crate::host::damage_full(state),
        Backend::Rawlink => crate::rawlink::damage_full(state),
    }
}

pub fn open_screen(state: &mut LiviState, screen_idx: usize) {
    match state.backend {
        Backend::Host => crate::host::open_screen(state, screen_idx),
        Backend::Rawlink if screen_idx != 0 => {
            log::info!("screen '{}' has no output in rawlink mode", state.screens[screen_idx].role);
        }
        Backend::Rawlink => {}
    }
}

pub fn close_screen(state: &mut LiviState, screen_idx: usize) {
    if state.backend == Backend::Host {
        crate::host::close_screen(state, screen_idx);
    }
}

pub fn set_fullscreen(state: &mut LiviState, screen_idx: usize, fullscreen: bool) {
    if state.backend == Backend::Host {
        crate::host::set_fullscreen(state, screen_idx, fullscreen);
    }
}

/// the rawlink panel stays fullscreen whatever a client asks for.
pub fn can_leave_fullscreen(state: &LiviState) -> bool {
    state.backend == Backend::Host
}

pub fn panel_mm(state: &LiviState, screen_idx: usize) -> Option<(i32, i32)> {
    match state.backend {
        Backend::Host => crate::host::panel_mm(state, screen_idx),
        // the head unit doesn't report its size
        Backend::Rawlink => None,
    }
}

pub fn import_dmabuf(state: &mut LiviState, dmabuf: &smithay::backend::allocator::dmabuf::Dmabuf) -> bool {
    match state.backend {
        Backend::Host => crate::host::import_dmabuf(state, dmabuf),
        Backend::Rawlink => crate::rawlink::import_dmabuf(state, dmabuf),
    }
}

/// the backend's share of each loop turn.
pub fn after_dispatch(state: &mut LiviState) {
    match state.backend {
        Backend::Host => {
            crate::host::apply_settled_resizes(state);
            crate::host::pump(state);
        }
        Backend::Rawlink => crate::rawlink::pump(state),
    }
}
