//! tuning constants for the backends, kept in one place so none of them turn
//! into scattered literals.

use std::time::Duration;

/// host window size when neither the ctrl socket nor LIVI_OUTPUT_SIZE gives one.
pub const HOST_DEFAULT_SIZE: (i32, i32) = (1280, 720);

/// the head unit panel, the rawlink output when LIVI_OUTPUT_SIZE is unset.
pub const RAWLINK_DEFAULT_SIZE: (i32, i32) = (800, 480);

/// refresh advertised on the wl_output, in mHz. clients only use it as a hint.
pub const OUTPUT_REFRESH_MHZ: i32 = 60_000;

/// keyboard repeat handed to clients (delay ms, rate per second).
pub const KEY_REPEAT_DELAY_MS: i32 = 600;
pub const KEY_REPEAT_RATE: i32 = 25;

pub const RAWLINK_DEFAULT_SOCK: &str = "/run/rawlink/frames.sock";

/// reconnect backoff for frames.sock. rawlink restarts in 0.5 s, so the cap sits
/// just above that and a restart costs at most one cap of dead time.
pub const RAWLINK_RETRY_FIRST: Duration = Duration::from_millis(50);
pub const RAWLINK_RETRY_MAX: Duration = Duration::from_millis(800);

/// past this many damage rects a FRAME carries their bounding box instead,
/// rawlink compares tiles itself so the extra area costs it little.
pub const MAX_DAMAGE_RECTS: usize = 64;

/// ordered dither matrix, anchored at screen (0, 0) and indexed [y & 3][x & 3].
/// the shader and the cpu pass both read it from here.
pub const BAYER4: [[u8; 4]; 4] = [
    [0, 8, 2, 10],
    [12, 4, 14, 6],
    [3, 11, 1, 9],
    [15, 7, 13, 5],
];

