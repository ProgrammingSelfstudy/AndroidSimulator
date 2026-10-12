//! The subset of the scrcpy (v5.0.1) wire protocol moniqi needs.
//!
//! Sources of truth (server side): `DesktopConnection`, `Streamer`, `ControlMessageReader`.
//! Everything is big-endian.
use std::io::{self, Read};

/// scrcpy server version this implementation was written against; the server refuses to start
/// if the version argument doesn't match its own.
pub const SERVER_VERSION: &str = "5.0.1";

pub const CODEC_H264: u32 = 0x6832_3634; // "h264"
pub const CODEC_H265: u32 = 0x6832_3635; // "h265"

const FLAG_CONFIG: u64 = 1 << 62;
const FLAG_KEY_FRAME: u64 = 1 << 61;
const PTS_MASK: u64 = FLAG_KEY_FRAME - 1;

// ---------------------------------------------------------------------------------------------
// Video stream
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct StreamHeader {
    pub device_name: String,
    pub codec: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Packet {
    /// The video size changed (rotation, resize). Frames after this use the new size.
    Session { width: u32, height: u32 },
    /// Codec configuration (H.264: Annex-B SPS+PPS).
    Config(Vec<u8>),
    Frame { pts_us: u64, key: bool, data: Vec<u8> },
}

/// First byte on the video socket (`send_dummy_byte`): proves the connection is live.
pub fn read_dummy_byte(r: &mut impl Read) -> io::Result<()> {
    let mut b = [0u8; 1];
    r.read_exact(&mut b)
}

/// The rest of the preamble: 64-byte device name and 4-byte codec id.
///
/// The server accepts the video *and* control sockets before it sends this, so a client must
/// open its control connection between [`read_dummy_byte`] and this call.
pub fn read_stream_meta(r: &mut impl Read) -> io::Result<StreamHeader> {
    let mut name = [0u8; 64];
    r.read_exact(&mut name)?;
    let end = name.iter().position(|&b| b == 0).unwrap_or(name.len());
    let mut codec = [0u8; 4];
    r.read_exact(&mut codec)?;
    Ok(StreamHeader {
        device_name: String::from_utf8_lossy(&name[..end]).into_owned(),
        codec: u32::from_be_bytes(codec),
    })
}

/// Whole preamble in one call, for callers that don't need to interleave a control connect.
pub fn read_stream_header(r: &mut impl Read) -> io::Result<StreamHeader> {
    read_dummy_byte(r)?;
    read_stream_meta(r)
}

/// Largest packet we accept; a 4K H.264 keyframe is well under this.
const MAX_PACKET: usize = 32 << 20;

pub fn read_packet(r: &mut impl Read) -> io::Result<Packet> {
    let mut hdr = [0u8; 12];
    r.read_exact(&mut hdr)?;
    if hdr[0] & 0x80 != 0 {
        // session packet: [flags u32][width u32][height u32]
        let width = u32::from_be_bytes([hdr[4], hdr[5], hdr[6], hdr[7]]);
        let height = u32::from_be_bytes([hdr[8], hdr[9], hdr[10], hdr[11]]);
        return Ok(Packet::Session { width, height });
    }
    let pts_flags = u64::from_be_bytes(hdr[0..8].try_into().unwrap());
    let size = u32::from_be_bytes(hdr[8..12].try_into().unwrap()) as usize;
    if size > MAX_PACKET {
        return Err(io::Error::new(io::ErrorKind::InvalidData, format!("packet of {size} bytes")));
    }
    let mut data = vec![0u8; size];
    r.read_exact(&mut data)?;
    if pts_flags & FLAG_CONFIG != 0 {
        Ok(Packet::Config(data))
    } else {
        Ok(Packet::Frame { pts_us: pts_flags & PTS_MASK, key: pts_flags & FLAG_KEY_FRAME != 0, data })
    }
}

// ---------------------------------------------------------------------------------------------
// Annex-B helpers (H.264)
// ---------------------------------------------------------------------------------------------

/// Split an Annex-B buffer into NAL units (start codes removed).
pub fn split_annexb(data: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new(); // (index of first payload byte, index of start code)
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            let sc = if i > 0 && data[i - 1] == 0 { i - 1 } else { i };
            starts.push((i + 3, sc));
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut out = Vec::new();
    for (n, &(payload, _)) in starts.iter().enumerate() {
        let end = starts.get(n + 1).map_or(data.len(), |&(_, sc)| sc);
        if payload < end {
            out.push(&data[payload..end]);
        }
    }
    out
}

/// H.264 NAL unit type (low 5 bits of the first byte).
pub fn nal_type(nal: &[u8]) -> Option<u8> {
    nal.first().map(|b| b & 0x1f)
}

pub const NAL_SPS: u8 = 7;
pub const NAL_PPS: u8 = 8;

/// Extract the first SPS and PPS from a config packet.
pub fn sps_pps(config: &[u8]) -> Option<(&[u8], &[u8])> {
    let nals = split_annexb(config);
    let sps = nals.iter().find(|n| nal_type(n) == Some(NAL_SPS))?;
    let pps = nals.iter().find(|n| nal_type(n) == Some(NAL_PPS))?;
    Some((sps, pps))
}

/// Convert an Annex-B access unit to AVCC (4-byte big-endian length prefixes), dropping
/// parameter-set NALs, which live in the format description instead.
pub fn annexb_to_avcc(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    for nal in split_annexb(data) {
        if matches!(nal_type(nal), Some(NAL_SPS) | Some(NAL_PPS)) {
            continue;
        }
        out.extend_from_slice(&(nal.len() as u32).to_be_bytes());
        out.extend_from_slice(nal);
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Control messages (client -> server)
// ---------------------------------------------------------------------------------------------

const TYPE_INJECT_KEYCODE: u8 = 0;
const TYPE_INJECT_TEXT: u8 = 1;
const TYPE_INJECT_TOUCH_EVENT: u8 = 2;
const TYPE_INJECT_SCROLL_EVENT: u8 = 3;
const TYPE_BACK_OR_SCREEN_ON: u8 = 4;
const TYPE_ROTATE_DEVICE: u8 = 11;

/// Android `MotionEvent` actions.
pub const ACTION_DOWN: u8 = 0;
pub const ACTION_UP: u8 = 1;
pub const ACTION_MOVE: u8 = 2;

/// Android `KeyEvent` actions.
pub const KEY_DOWN: u8 = 0;
pub const KEY_UP: u8 = 1;

/// Pointer id scrcpy uses for a generic finger (as opposed to -1 = mouse).
pub const POINTER_FINGER: u64 = u64::MAX - 1;

/// Insets the injected position is relative to: the size of the frame the client displayed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Position {
    pub x: i32,
    pub y: i32,
    pub width: u16,
    pub height: u16,
}

fn put_position(buf: &mut Vec<u8>, p: Position) {
    buf.extend_from_slice(&p.x.to_be_bytes());
    buf.extend_from_slice(&p.y.to_be_bytes());
    buf.extend_from_slice(&p.width.to_be_bytes());
    buf.extend_from_slice(&p.height.to_be_bytes());
}

/// 0.0..=1.0 as an unsigned 16-bit fixed point (1.0 is 0xffff).
fn u16_fixed(v: f32) -> u16 {
    (v.clamp(0.0, 1.0) * 65535.0).round() as u16
}

/// -1.0..=1.0 as signed 16-bit fixed point.
fn i16_fixed(v: f32) -> i16 {
    (v.clamp(-1.0, 1.0) * 32767.0).round() as i16
}

pub fn touch(action: u8, pointer_id: u64, pos: Position, pressure: f32, buttons: u32) -> Vec<u8> {
    let mut b = Vec::with_capacity(32);
    b.push(TYPE_INJECT_TOUCH_EVENT);
    b.push(action);
    b.extend_from_slice(&pointer_id.to_be_bytes());
    put_position(&mut b, pos);
    b.extend_from_slice(&u16_fixed(pressure).to_be_bytes());
    b.extend_from_slice(&0u32.to_be_bytes()); // action button
    b.extend_from_slice(&buttons.to_be_bytes());
    b
}

pub fn key(action: u8, keycode: u32, repeat: u32, meta_state: u32) -> Vec<u8> {
    let mut b = Vec::with_capacity(14);
    b.push(TYPE_INJECT_KEYCODE);
    b.push(action);
    b.extend_from_slice(&keycode.to_be_bytes());
    b.extend_from_slice(&repeat.to_be_bytes());
    b.extend_from_slice(&meta_state.to_be_bytes());
    b
}

/// Text is capped at 300 bytes server-side; longer input is truncated on a char boundary.
pub fn text(s: &str) -> Vec<u8> {
    let mut end = s.len().min(300);
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    let t = &s.as_bytes()[..end];
    let mut b = Vec::with_capacity(5 + t.len());
    b.push(TYPE_INJECT_TEXT);
    b.extend_from_slice(&(t.len() as u32).to_be_bytes());
    b.extend_from_slice(t);
    b
}

/// `h`/`v` are in "notches"; the server scales the fixed-point value by 16.
pub fn scroll(pos: Position, h: f32, v: f32, buttons: u32) -> Vec<u8> {
    let mut b = Vec::with_capacity(21);
    b.push(TYPE_INJECT_SCROLL_EVENT);
    put_position(&mut b, pos);
    b.extend_from_slice(&i16_fixed(h / 16.0).to_be_bytes());
    b.extend_from_slice(&i16_fixed(v / 16.0).to_be_bytes());
    b.extend_from_slice(&buttons.to_be_bytes());
    b
}

pub fn back_or_screen_on(action: u8) -> Vec<u8> {
    vec![TYPE_BACK_OR_SCREEN_ON, action]
}

pub fn rotate_device() -> Vec<u8> {
    vec![TYPE_ROTATE_DEVICE]
}

/// Android keycodes (`android.view.KeyEvent`) used by the host keyboard mapping.
pub mod keycode {
    pub const HOME: u32 = 3;
    pub const BACK: u32 = 4;
    pub const DPAD_UP: u32 = 19;
    pub const DPAD_DOWN: u32 = 20;
    pub const DPAD_LEFT: u32 = 21;
    pub const DPAD_RIGHT: u32 = 22;
    pub const ENTER: u32 = 66;
    pub const DEL: u32 = 67; // backspace
    pub const TAB: u32 = 61;
    pub const ESCAPE: u32 = 111;
    pub const FORWARD_DEL: u32 = 112;
    pub const MOVE_HOME: u32 = 122;
    pub const MOVE_END: u32 = 123;
    pub const PAGE_UP: u32 = 92;
    pub const PAGE_DOWN: u32 = 93;
    pub const MENU: u32 = 82;
    pub const APP_SWITCH: u32 = 187;
    pub const VOLUME_UP: u32 = 24;
    pub const VOLUME_DOWN: u32 = 25;
    pub const POWER: u32 = 26;
    pub const SPACE: u32 = 62;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn pos() -> Position {
        Position { x: 100, y: 200, width: 720, height: 1280 }
    }

    #[test]
    fn touch_layout_matches_server_reader() {
        let m = touch(ACTION_DOWN, POINTER_FINGER, pos(), 1.0, 1);
        assert_eq!(m.len(), 32);
        assert_eq!(m[0], 2); // TYPE_INJECT_TOUCH_EVENT
        assert_eq!(m[1], ACTION_DOWN);
        assert_eq!(&m[2..10], &[0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xfe]); // -2
        assert_eq!(&m[10..14], &100i32.to_be_bytes());
        assert_eq!(&m[14..18], &200i32.to_be_bytes());
        assert_eq!(&m[18..20], &720u16.to_be_bytes());
        assert_eq!(&m[20..22], &1280u16.to_be_bytes());
        assert_eq!(&m[22..24], &[0xff, 0xff]); // pressure 1.0
        assert_eq!(&m[28..32], &1u32.to_be_bytes()); // buttons
    }

    #[test]
    fn key_text_scroll_layouts() {
        let k = key(KEY_DOWN, keycode::BACK, 0, 0);
        assert_eq!(k.len(), 14);
        assert_eq!(&k[..2], &[0, 0]);
        assert_eq!(&k[2..6], &4u32.to_be_bytes());

        let t = text("héllo");
        assert_eq!(t[0], 1);
        assert_eq!(&t[1..5], &6u32.to_be_bytes()); // é is 2 bytes
        assert_eq!(&t[5..], "héllo".as_bytes());
        // truncation never splits a char
        let long = "é".repeat(200);
        let t = text(&long);
        assert!(std::str::from_utf8(&t[5..]).is_ok());
        assert!(t.len() - 5 <= 300);

        let s = scroll(pos(), 0.0, -1.0, 0);
        assert_eq!(s.len(), 21);
        assert_eq!(s[0], 3);
        let v = i16::from_be_bytes([s[15], s[16]]);
        assert!(v < 0);
    }

    fn stream_bytes() -> Vec<u8> {
        let mut v = vec![0u8]; // dummy
        let mut name = b"redroid".to_vec();
        name.resize(64, 0);
        v.extend(name);
        v.extend(CODEC_H264.to_be_bytes());
        // session
        v.extend((1u32 << 31).to_be_bytes());
        v.extend(720u32.to_be_bytes());
        v.extend(1280u32.to_be_bytes());
        // config packet (SPS+PPS)
        let cfg = [0, 0, 0, 1, 0x67, 1, 2, 3, 0, 0, 0, 1, 0x68, 4, 5];
        v.extend(FLAG_CONFIG.to_be_bytes());
        v.extend((cfg.len() as u32).to_be_bytes());
        v.extend(cfg);
        // key frame
        let frame = [0, 0, 0, 1, 0x65, 9, 9];
        v.extend((FLAG_KEY_FRAME | 1234).to_be_bytes());
        v.extend((frame.len() as u32).to_be_bytes());
        v.extend(frame);
        v
    }

    #[test]
    fn parses_stream() {
        let mut c = Cursor::new(stream_bytes());
        let h = read_stream_header(&mut c).unwrap();
        assert_eq!(h, StreamHeader { device_name: "redroid".into(), codec: CODEC_H264 });
        assert_eq!(read_packet(&mut c).unwrap(), Packet::Session { width: 720, height: 1280 });
        let cfg = match read_packet(&mut c).unwrap() {
            Packet::Config(d) => d,
            p => panic!("expected config, got {p:?}"),
        };
        let (sps, pps) = sps_pps(&cfg).unwrap();
        assert_eq!(sps, &[0x67, 1, 2, 3]);
        assert_eq!(pps, &[0x68, 4, 5]);
        match read_packet(&mut c).unwrap() {
            Packet::Frame { pts_us, key, data } => {
                assert_eq!(pts_us, 1234);
                assert!(key);
                assert_eq!(annexb_to_avcc(&data), vec![0, 0, 0, 3, 0x65, 9, 9]);
            }
            p => panic!("expected frame, got {p:?}"),
        }
        assert!(read_packet(&mut c).is_err()); // clean EOF -> error, not a hang
    }

    #[test]
    fn annexb_handles_three_and_four_byte_start_codes() {
        let d = [0, 0, 1, 0x41, 1, 2, 0, 0, 0, 1, 0x41, 3];
        let n = split_annexb(&d);
        assert_eq!(n, vec![&[0x41, 1, 2][..], &[0x41, 3][..]]);
    }

    #[test]
    fn rejects_absurd_packet_sizes() {
        let mut v = 0u64.to_be_bytes().to_vec();
        v.extend(u32::MAX.to_be_bytes());
        assert!(read_packet(&mut Cursor::new(v)).is_err());
    }
}
