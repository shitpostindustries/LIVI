//! frames.sock wire format (rawplay.md 2.7). seqpacket, one message per
//! packet, every message opens with `type u8, reserved[3]`, little endian.

use std::os::fd::{AsRawFd, OwnedFd, RawFd};

use nix::sys::socket::{
    self, AddressFamily, ControlMessage, MsgFlags, SockFlag, SockType, UnixAddr,
};

const HELLO: u8 = 1;
const FRAME: u8 = 2;
const GRANT: u8 = 3;
const TOUCH: u8 = 4;
const KEY: u8 = 5;

const VERSION: u32 = 2;
const FORMAT_RGB565: u32 = 1;
pub const FLAG_FULL: u16 = 1;

/// what rawlink can send us.
#[derive(Debug)]
pub enum Incoming {
    Grant(u32),
    Touch { x: u16, y: u16, down: bool },
    Key { code: u32, pressed: bool },
}

fn u16_at(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

/// None for a short or unknown message, those are skipped rather than fatal.
pub fn parse(b: &[u8]) -> Option<Incoming> {
    match *b.first()? {
        GRANT if b.len() >= 8 => Some(Incoming::Grant(u32_at(b, 4))),
        TOUCH if b.len() >= 12 => Some(Incoming::Touch {
            x: u16_at(b, 4),
            y: u16_at(b, 6),
            down: b[8] != 0,
        }),
        KEY if b.len() >= 12 => Some(Incoming::Key {
            code: u32_at(b, 4),
            pressed: b[8] != 0,
        }),
        _ => None,
    }
}

fn header(kind: u8) -> Vec<u8> {
    vec![kind, 0, 0, 0]
}

pub fn hello(width: u16, height: u16, stride: u32) -> Vec<u8> {
    let mut m = header(HELLO);
    m.extend_from_slice(&VERSION.to_le_bytes());
    m.extend_from_slice(&width.to_le_bytes());
    m.extend_from_slice(&height.to_le_bytes());
    m.extend_from_slice(&FORMAT_RGB565.to_le_bytes());
    m.extend_from_slice(&stride.to_le_bytes());
    m
}

/// each damage rect is `x y w h`.
pub fn frame(seq: u32, ts_ns: u64, flags: u16, damage: &[[u16; 4]]) -> Vec<u8> {
    let mut m = header(FRAME);
    m.extend_from_slice(&seq.to_le_bytes());
    m.extend_from_slice(&0u32.to_le_bytes());
    m.extend_from_slice(&ts_ns.to_le_bytes());
    m.extend_from_slice(&(damage.len() as u16).to_le_bytes());
    m.extend_from_slice(&flags.to_le_bytes());
    for r in damage {
        for v in r {
            m.extend_from_slice(&v.to_le_bytes());
        }
    }
    m
}

pub fn connect(path: &str) -> nix::Result<OwnedFd> {
    let fd = socket::socket(
        AddressFamily::Unix,
        SockType::SeqPacket,
        SockFlag::SOCK_CLOEXEC | SockFlag::SOCK_NONBLOCK,
        None,
    )?;
    socket::connect(fd.as_raw_fd(), &UnixAddr::new(path)?)?;
    Ok(fd)
}

/// the hello carries the frame memfd along with it.
pub fn send_hello(fd: RawFd, msg: &[u8], memfd: RawFd) -> nix::Result<()> {
    let iov = [std::io::IoSlice::new(msg)];
    let fds = [memfd];
    socket::sendmsg::<UnixAddr>(fd, &iov, &[ControlMessage::ScmRights(&fds)], MsgFlags::MSG_NOSIGNAL, None)?;
    Ok(())
}

pub fn send(fd: RawFd, msg: &[u8]) -> nix::Result<()> {
    socket::send(fd, msg, MsgFlags::MSG_NOSIGNAL | MsgFlags::MSG_DONTWAIT)?;
    Ok(())
}
