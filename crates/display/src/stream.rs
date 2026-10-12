//! Video receive loop: scrcpy stream in, sample buffers out to the display layer.
use std::io;
use std::net::TcpStream;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use moniqi_scrcpy::{self as sc, Packet};
use objc2::encode::{Encoding, RefEncode};
use objc2::msg_send;
use objc2::runtime::AnyObject;

use crate::cm;
use crate::window::Shared;

#[repr(C)]
pub struct OpaqueCMSampleBuffer {
    _private: [u8; 0],
}

// SAFETY: matches the framework's `CMSampleBufferRef` (`^{opaqueCMSampleBuffer=}`).
unsafe impl RefEncode for OpaqueCMSampleBuffer {
    const ENCODING_REF: Encoding = Encoding::Pointer(&Encoding::Struct("opaqueCMSampleBuffer", &[]));
}

fn enqueue(layer: &AnyObject, sample: &cm::Cf) {
    // SAFETY: AVSampleBufferDisplayLayer's enqueue is documented as thread-safe; `sample` is a
    // valid CMSampleBuffer retained by `sample`'s owner for the duration of the call.
    unsafe {
        let status: isize = msg_send![layer, status];
        if status == 2 {
            // AVQueuedSampleBufferRenderingStatusFailed: recover instead of freezing forever.
            let _: () = msg_send![layer, flush];
        }
        let sb = sample.as_ptr() as *mut OpaqueCMSampleBuffer;
        let _: () = msg_send![layer, enqueueSampleBuffer: sb];
    }
}

fn one_session(shared: &Shared, video_addr: &str, control_addr: &str) -> io::Result<()> {
    let mut v = TcpStream::connect(video_addr)?;
    v.set_nodelay(true)?;
    // The forward accepts instantly; the dummy byte only arrives once the guest server is up.
    v.set_read_timeout(Some(Duration::from_secs(40)))?;
    sc::read_dummy_byte(&mut v)?;

    let c = TcpStream::connect(control_addr)?;
    c.set_nodelay(true)?;
    *shared.control.lock().unwrap() = Some(c);

    let meta = sc::read_stream_meta(&mut v)?;
    if meta.codec != sc::CODEC_H264 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, format!("unsupported codec {:#x}", meta.codec)));
    }
    v.set_read_timeout(None)?;
    shared.connected.store(true, Ordering::Relaxed);

    let mut fmt: Option<cm::Cf> = None;
    loop {
        match sc::read_packet(&mut v)? {
            Packet::Session { width, height } => {
                shared.width.store(width, Ordering::Relaxed);
                shared.height.store(height, Ordering::Relaxed);
            }
            Packet::Config(data) => {
                if let Some((sps, pps)) = sc::sps_pps(&data) {
                    match cm::format_description(sps, pps) {
                        Ok(f) => fmt = Some(f),
                        Err(st) => eprintln!("display: format description failed: {st}"),
                    }
                }
            }
            Packet::Frame { pts_us, data, .. } => {
                let Some(f) = fmt.as_ref() else { continue };
                let avcc = sc::annexb_to_avcc(&data);
                if avcc.is_empty() {
                    continue;
                }
                match cm::sample_buffer(f, &avcc, pts_us) {
                    Ok(sb) => {
                        enqueue(shared.layer(), &sb);
                        shared.frames.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(st) => eprintln!("display: sample buffer failed: {st}"),
                }
            }
        }
    }
}

/// Keep a session alive for the life of the app: the guest takes a while to boot, and the
/// server restarts whenever the window reconnects.
pub fn spawn(shared: Arc<Shared>, video_addr: String, control_addr: String) {
    thread::spawn(move || {
        while !shared.quit.load(Ordering::Relaxed) {
            let r = one_session(&shared, &video_addr, &control_addr);
            shared.connected.store(false, Ordering::Relaxed);
            *shared.control.lock().unwrap() = None;
            if let Err(e) = r {
                // Read errors after a clean disconnect are expected; keep quiet about timeouts.
                if e.kind() != io::ErrorKind::UnexpectedEof && e.kind() != io::ErrorKind::ConnectionRefused {
                    eprintln!("display: stream: {e}");
                }
            }
            thread::sleep(Duration::from_millis(1000));
        }
    });
}
