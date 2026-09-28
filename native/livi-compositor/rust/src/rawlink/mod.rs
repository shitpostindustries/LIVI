//! headless rawlink backend. one output of LIVI_OUTPUT_SIZE rendered offscreen
//! as dithered rgb565 into a memfd shared with rawlink over frames.sock, and
//! rawlink's touch and keys injected straight into our seat.
//!
//! pacing is render on grant. a frame is rendered only while rawlink holds out a
//! grant and something changed, and client frame callbacks go out once per
//! published frame, so clients produce at the link's rate.

mod dither;
mod offscreen;
mod proto;

use std::num::NonZeroUsize;
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::ptr::NonNull;
use std::time::Duration;

use nix::sys::mman::{mmap, MapFlags, ProtFlags};
use nix::sys::socket::MsgFlags;
use smithay::backend::input::KeyState;
use smithay::input::keyboard::FilterResult;
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::reexports::calloop::{Interest, LoopHandle, Mode, PostAction, RegistrationToken};
use smithay::utils::SERIAL_COUNTER;

use crate::render::Scene;
use crate::state::LiviState;
use crate::tuning::{
    KEY_REPEAT_DELAY_MS, KEY_REPEAT_RATE, MAX_DAMAGE_RECTS, RAWLINK_DEFAULT_SIZE, RAWLINK_DEFAULT_SOCK,
    RAWLINK_RETRY_FIRST, RAWLINK_RETRY_MAX,
};
use dither::Rect;
use offscreen::Offscreen;

/// the whole current frame, shared with rawlink. rawlink copies the damaged
/// rects out before it grants again, so one buffer is enough.
struct FrameBuf {
    fd: OwnedFd,
    ptr: NonNull<u8>,
    len: usize,
    stride: usize,
}

impl FrameBuf {
    fn new(width: usize, height: usize) -> Result<Self, String> {
        let stride = width * 2;
        let len = stride * height;
        let fd = nix::sys::memfd::memfd_create(c"livi-frame", nix::sys::memfd::MemFdCreateFlag::MFD_CLOEXEC)
            .map_err(|e| format!("memfd_create failed, {e}"))?;
        nix::unistd::ftruncate(&fd, len as i64).map_err(|e| format!("memfd ftruncate failed, {e}"))?;
        let ptr = unsafe {
            mmap(
                None,
                NonZeroUsize::new(len).ok_or("empty output")?,
                ProtFlags::PROT_READ | ProtFlags::PROT_WRITE,
                MapFlags::MAP_SHARED,
                fd.as_fd(),
                0,
            )
        }
        .map_err(|e| format!("memfd mmap failed, {e}"))?;
        Ok(Self { fd, ptr: ptr.cast(), len, stride })
    }

    fn pixels(&mut self) -> &mut [u8] {
        // the mapping lives as long as self and nothing else in this process touches it
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
    }
}

struct Link {
    fd: OwnedFd,
    token: RegistrationToken,
}

pub struct Rawlink {
    path: String,
    size: (i32, i32),
    offscreen: Offscreen,
    frame: FrameBuf,
    handle: LoopHandle<'static, LiviState>,
    link: Option<Link>,
    retry: Duration,
    retrying: bool,
    // quiet retries after the first "not up yet"
    announced_down: bool,
    granted: bool,
    dirty: bool,
    // the first frame after HELLO carries the whole screen
    full: bool,
    // the panel's physical size from LIVI_OUTPUT_MM, the head unit can't report it
    panel_mm: Option<(i32, i32)>,
    // CLOCK_MONOTONIC of the oldest change not yet published
    oldest_ns: Option<u64>,
    seq: u32,
    touching: bool,
}

fn monotonic_ns() -> u64 {
    let t = nix::time::clock_gettime(nix::time::ClockId::CLOCK_MONOTONIC).unwrap_or(nix::sys::time::TimeSpec::new(0, 0));
    t.tv_sec() as u64 * 1_000_000_000 + t.tv_nsec() as u64
}

fn event_time_ms() -> u32 {
    (monotonic_ns() / 1_000_000) as u32
}

fn output_size() -> (i32, i32) {
    crate::backend::output_size_env().unwrap_or(RAWLINK_DEFAULT_SIZE)
}

pub fn init(state: &mut LiviState, handle: &LoopHandle<'static, LiviState>) {
    let size = output_size();
    let offscreen = offscreen::select(size).unwrap_or_else(|e| {
        log::error!("{e}");
        std::process::exit(1);
    });
    let frame = FrameBuf::new(size.0 as usize, size.1 as usize).unwrap_or_else(|e| {
        log::error!("{e}");
        std::process::exit(1);
    });

    // one fixed fullscreen output, no decorations and no other screens
    let s = &mut state.screens[0];
    s.width = size.0;
    s.height = size.1;
    s.applied_width = size.0;
    s.applied_height = size.1;
    s.fullscreen = true;
    crate::host::ensure_server_output(state, 0);

    state.seat.add_touch();
    if let Err(e) = state.seat.add_keyboard(Default::default(), KEY_REPEAT_DELAY_MS, KEY_REPEAT_RATE) {
        log::error!("rawlink keyboard failed, {e}");
    }

    // pixman keeps dmabuf too, gst 1.28 waylandsink's shm path reports the unpadded stride
    if let Some(formats) = offscreen.dmabuf_formats() {
        let _ = state.dmabuf_state.create_global::<LiviState>(&state.display_handle, formats);
    } else {
        log::info!("linux-dmabuf not advertised, the {} tier can't import it", offscreen.tier.name());
    }

    let path = std::env::var("LIVI_RAWLINK_SOCK")
        .ok()
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| RAWLINK_DEFAULT_SOCK.to_string());
    log::info!("rawlink output {}x{}, frames.sock at {path}", size.0, size.1);
    let panel_mm = crate::backend::size_env("LIVI_OUTPUT_MM");
    if panel_mm.is_none() {
        log::info!("LIVI_OUTPUT_MM unset, no panel size goes to LIVI");
    }
    state.rawlink = Some(Rawlink {
        path,
        size,
        offscreen,
        frame,
        handle: handle.clone(),
        link: None,
        retry: RAWLINK_RETRY_FIRST,
        retrying: false,
        announced_down: false,
        granted: false,
        dirty: true,
        full: true,
        panel_mm,
        oldest_ns: Some(monotonic_ns()),
        seq: 0,
        touching: false,
    });
    if !try_connect(state) {
        schedule_retry(state);
    }
}

fn try_connect(state: &mut LiviState) -> bool {
    let Some(rl) = state.rawlink.as_mut() else { return false };
    let fd = match proto::connect(&rl.path) {
        Ok(fd) => fd,
        Err(e) => {
            if !rl.announced_down {
                log::info!("rawlink not up at {} ({e}), retrying", rl.path);
                rl.announced_down = true;
            }
            return false;
        }
    };
    let hello = proto::hello(rl.size.0 as u16, rl.size.1 as u16, rl.frame.stride as u32);
    if let Err(e) = proto::send_hello(fd.as_raw_fd(), &hello, rl.frame.fd.as_raw_fd()) {
        log::warn!("rawlink hello failed, {e}");
        return false;
    }
    let watch = match fd.try_clone() {
        Ok(w) => w,
        Err(e) => {
            log::warn!("rawlink socket dup failed, {e}");
            return false;
        }
    };
    let token = rl.handle.insert_source(Generic::new(watch, Interest::READ, Mode::Level), |_, _, state| {
        Ok(if on_readable(state) { PostAction::Continue } else { PostAction::Remove })
    });
    let token = match token {
        Ok(t) => t,
        Err(e) => {
            log::warn!("rawlink socket watch failed, {e}");
            return false;
        }
    };
    log::info!("rawlink connected at {}", rl.path);
    rl.link = Some(Link { fd, token });
    rl.retry = RAWLINK_RETRY_FIRST;
    rl.announced_down = false;
    rl.granted = false;
    rl.full = true;
    rl.dirty = true;
    rl.oldest_ns.get_or_insert_with(monotonic_ns);
    true
}

fn schedule_retry(state: &mut LiviState) {
    let Some(rl) = state.rawlink.as_mut() else { return };
    if rl.retrying {
        return;
    }
    rl.retrying = true;
    let first = rl.retry;
    let res = rl.handle.insert_source(Timer::from_duration(first), |_, _, state: &mut LiviState| {
        if try_connect(state) {
            if let Some(rl) = state.rawlink.as_mut() {
                rl.retrying = false;
            }
            return TimeoutAction::Drop;
        }
        let Some(rl) = state.rawlink.as_mut() else { return TimeoutAction::Drop };
        rl.retry = (rl.retry * 2).min(RAWLINK_RETRY_MAX);
        TimeoutAction::ToDuration(rl.retry)
    });
    if let Err(e) = res {
        log::error!("rawlink retry timer failed, {e}");
    }
}

/// drops the link. `watched` is false when called from the socket's own
/// source, which removes itself by returning Remove.
fn disconnect(state: &mut LiviState, why: &str, watched: bool) {
    let Some(rl) = state.rawlink.as_mut() else { return };
    let Some(link) = rl.link.take() else { return };
    if watched {
        rl.handle.remove(link.token);
    }
    rl.granted = false;
    log::info!("rawlink gone ({why}), reconnecting");
    // touches in flight would never see their release otherwise
    if rl.touching {
        rl.touching = false;
        crate::input::touch_cancel(state);
    }
    schedule_retry(state);
}

/// reads every waiting message. false once the link is gone.
fn on_readable(state: &mut LiviState) -> bool {
    let mut buf = [0u8; 64];
    loop {
        let Some(fd) = state.rawlink.as_ref().and_then(|r| r.link.as_ref()).map(|l| l.fd.as_raw_fd()) else {
            return false;
        };
        match nix::sys::socket::recv(fd, &mut buf, MsgFlags::MSG_DONTWAIT) {
            Ok(0) => {
                disconnect(state, "closed", false);
                return false;
            }
            Ok(n) => match proto::parse(&buf[..n]) {
                Some(msg) => handle(state, msg),
                None => log::debug!("rawlink sent an unknown or short message ({n} bytes), skipped"),
            },
            Err(nix::errno::Errno::EAGAIN) => return true,
            Err(nix::errno::Errno::EINTR) => continue,
            Err(e) => {
                disconnect(state, &e.to_string(), false);
                return false;
            }
        }
    }
}

fn handle(state: &mut LiviState, msg: proto::Incoming) {
    match msg {
        proto::Incoming::Grant(g) => {
            log::trace!("rawlink grant {g}");
            if let Some(rl) = state.rawlink.as_mut() {
                rl.granted = true;
            }
        }
        proto::Incoming::Touch { x, y, down } => touch(state, x as f64, y as f64, down),
        proto::Incoming::Key { code, pressed } => key(state, code, pressed),
    }
}

/// panel coordinates are output coordinates, and the output is screen 0 at x 0.
/// down after up presses, down after down moves, up releases.
fn touch(state: &mut LiviState, x: f64, y: f64, down: bool) {
    let Some(rl) = state.rawlink.as_mut() else { return };
    let was = rl.touching;
    rl.touching = down;
    let time = event_time_ms();
    match (was, down) {
        (false, true) => crate::input::touch_down(state, time, 0, x, y, 0),
        (true, true) => crate::input::touch_motion(state, time, 0, x, y),
        (true, false) => crate::input::touch_up(state, time, 0),
        (false, false) => {}
    }
}

fn key(state: &mut LiviState, code: u32, pressed: bool) {
    let Some(keyboard) = state.seat.get_keyboard() else { return };
    keyboard.input::<(), _>(
        state,
        // xkb keycodes are evdev + 8
        (code + 8).into(),
        if pressed { KeyState::Pressed } else { KeyState::Released },
        SERIAL_COUNTER.next_serial(),
        event_time_ms(),
        |_, _, _| FilterResult::Forward,
    );
}

/// the one output is screen 0, nothing else has a panel.
pub fn panel_mm(state: &LiviState, screen_idx: usize) -> Option<(i32, i32)> {
    if screen_idx != 0 {
        return None;
    }
    state.rawlink.as_ref()?.panel_mm
}

pub fn damage(state: &mut LiviState) {
    if let Some(rl) = state.rawlink.as_mut() {
        rl.dirty = true;
        rl.oldest_ns.get_or_insert_with(monotonic_ns);
    }
}

pub fn damage_full(state: &mut LiviState) {
    if let Some(rl) = state.rawlink.as_mut() {
        rl.offscreen.reset();
    }
    damage(state);
}

pub fn import_dmabuf(state: &mut LiviState, dmabuf: &smithay::backend::allocator::dmabuf::Dmabuf) -> bool {
    state
        .rawlink
        .as_mut()
        .is_some_and(|rl| rl.offscreen.import_dmabuf(dmabuf))
}

/// from the loop turn. renders and publishes when a grant and a change meet.
pub fn pump(state: &mut LiviState) {
    let Some(rl) = state.rawlink.as_ref() else { return };
    if rl.link.is_none() || !rl.granted || !(rl.dirty || rl.full) {
        return;
    }
    publish(state);
}

fn to_u16_rect(r: &Rect) -> [u16; 4] {
    [r.x as u16, r.y as u16, r.w as u16, r.h as u16]
}

fn bounding(rects: &[Rect]) -> Rect {
    let x0 = rects.iter().map(|r| r.x).min().unwrap_or(0);
    let y0 = rects.iter().map(|r| r.y).min().unwrap_or(0);
    let x1 = rects.iter().map(|r| r.x + r.w).max().unwrap_or(0);
    let y1 = rects.iter().map(|r| r.y + r.h).max().unwrap_or(0);
    Rect { x: x0, y: y0, w: x1 - x0, h: y1 - y0 }
}

fn publish(state: &mut LiviState) {
    let clear = crate::render::backdrop_color(&state.screens[0]);
    let LiviState { rawlink, screens, toplevels, video_order, cal, .. } = state;
    let rl = rawlink.as_mut().unwrap();
    let scene = Scene::new(screens, toplevels, video_order);
    let full = rl.full;
    let stride = rl.frame.stride;
    let res = rl.offscreen.render(&scene, clear, cal, full, rl.frame.pixels(), stride);
    rl.dirty = false;
    let rects = match res {
        Ok(Some(rects)) => rects,
        Ok(None) => {
            // nothing visible changed. the grant stays held, and the clients
            // that committed get their callbacks since there's no frame to wait for
            rl.oldest_ns = None;
            crate::render::send_frame_callbacks(state);
            return;
        }
        Err(e) => {
            log::error!("rawlink render failed, {e}");
            return;
        }
    };
    let rects = if rects.len() > MAX_DAMAGE_RECTS { vec![bounding(&rects)] } else { rects };
    let damage: Vec<[u16; 4]> = rects.iter().map(to_u16_rect).collect();
    let ts = rl.oldest_ns.take().unwrap_or_else(monotonic_ns);
    let flags = if full { proto::FLAG_FULL } else { 0 };
    let msg = proto::frame(rl.seq, ts, flags, &damage);
    let fd = rl.link.as_ref().map(|l| l.fd.as_raw_fd());
    match fd.map(|fd| proto::send(fd, &msg)) {
        Some(Ok(())) => {
            log::debug!("rawlink frame {} with {} rects{}", rl.seq, damage.len(), if full { " (full)" } else { "" });
            rl.seq = rl.seq.wrapping_add(1);
            rl.granted = false;
            rl.full = false;
            crate::render::send_frame_callbacks(state);
        }
        Some(Err(e)) => disconnect(state, &format!("frame send failed, {e}"), true),
        None => {}
    }
}
