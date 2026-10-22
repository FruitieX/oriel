//! Clipboard sync: text and images. arboard has no change notifications,
//! so a thread polls (images less often, and on Windows only when the
//! clipboard's sequence number changed).

use std::{
    hash::{Hash, Hasher},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

/// Largest text synced, to keep the control stream responsive.
pub const MAX_TEXT: usize = 1 << 20;
/// Largest image synced (PNG bytes).
pub const MAX_IMAGE: usize = 24 << 20;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Content {
    Text(String),
    /// A PNG image.
    Image(Vec<u8>),
}

pub struct Options {
    /// Sync text too (the client handles text through its window system).
    pub text: bool,
    /// Poll only while this is set (e.g. while the viewer has focus).
    pub active: Option<Arc<AtomicBool>>,
}

pub struct ClipboardSync {
    /// Content from the peer to put on the local clipboard.
    pub set: mpsc::Sender<Content>,
}

/// Start syncing; `on_change` gets local clipboard changes.
pub fn start(options: Options, on_change: impl Fn(Content) + Send + 'static) -> ClipboardSync {
    let (set_tx, set_rx) = mpsc::channel::<Content>();
    std::thread::Builder::new()
        .name("oriel-clipboard".into())
        .spawn(move || run(options, set_rx, on_change))
        .expect("spawn clipboard thread");
    ClipboardSync { set: set_tx }
}

fn run(options: Options, set_rx: mpsc::Receiver<Content>, on_change: impl Fn(Content)) {
    let mut clipboard = match arboard::Clipboard::new() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("clipboard unavailable: {e}");
            return;
        }
    };
    let mut last_text = if options.text {
        clipboard.get_text().ok()
    } else {
        None
    };
    // What is on the clipboard now, so neither startup contents nor what
    // the peer just sent is sent back.
    let mut last_image = clipboard.get_image().ok().map(|i| image_hash(&i));
    let mut last_image_poll = Instant::now();
    let mut last_seq = sequence_number();
    loop {
        match set_rx.recv_timeout(Duration::from_millis(300)) {
            Ok(Content::Text(text)) => {
                if clipboard.set_text(text.clone()).is_ok() {
                    last_text = Some(text);
                }
                last_seq = sequence_number();
                continue;
            }
            Ok(Content::Image(png)) => {
                match decode_png(&png) {
                    Some(image) => {
                        last_image = Some(image_hash(&image));
                        if let Err(e) = clipboard.set_image(image) {
                            tracing::debug!("cannot set clipboard image: {e}");
                        }
                    }
                    None => tracing::debug!("received an unreadable clipboard image"),
                }
                last_seq = sequence_number();
                continue;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        if options
            .active
            .as_ref()
            .is_some_and(|a| !a.load(Ordering::Relaxed))
        {
            continue;
        }
        // Windows tells cheaply whether anything changed.
        let seq = sequence_number();
        if seq.is_some() && seq == last_seq {
            continue;
        }
        last_seq = seq;

        if options.text {
            let text = clipboard.get_text().ok();
            if text.is_some() && text != last_text {
                last_text = text.clone();
                if let Some(text) = text.filter(|t| t.len() <= MAX_TEXT) {
                    on_change(Content::Text(text));
                }
                continue;
            }
        }
        // Images are costly to read; look at most once a second.
        if seq.is_none() && last_image_poll.elapsed() < Duration::from_secs(1) {
            continue;
        }
        last_image_poll = Instant::now();
        if let Ok(image) = clipboard.get_image() {
            let hash = image_hash(&image);
            if last_image != Some(hash) {
                last_image = Some(hash);
                match encode_png(&image) {
                    Some(png) if png.len() <= MAX_IMAGE => on_change(Content::Image(png)),
                    Some(png) => {
                        tracing::info!(bytes = png.len(), "clipboard image too large to sync")
                    }
                    None => {}
                }
            }
        }
    }
}

/// The clipboard's change counter, where the platform has one.
fn sequence_number() -> Option<u32> {
    #[cfg(windows)]
    return Some(unsafe { windows::Win32::System::DataExchange::GetClipboardSequenceNumber() });
    #[cfg(not(windows))]
    None
}

fn image_hash(image: &arboard::ImageData) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    (image.width, image.height).hash(&mut h);
    image.bytes.hash(&mut h);
    h.finish()
}

fn encode_png(image: &arboard::ImageData) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut encoder = png::Encoder::new(&mut out, image.width as u32, image.height as u32);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.set_compression(png::Compression::Fast);
    let mut writer = encoder.write_header().ok()?;
    writer.write_image_data(&image.bytes).ok()?;
    writer.finish().ok()?;
    Some(out)
}

fn decode_png(png: &[u8]) -> Option<arboard::ImageData<'static>> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(png));
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0; reader.output_buffer_size()?];
    let info = reader.next_frame(&mut buf).ok()?;
    buf.truncate(info.buffer_size());
    let rgba = match info.color_type {
        png::ColorType::Rgba => buf,
        png::ColorType::Rgb => buf
            .chunks(3)
            .flat_map(|p| [p[0], p[1], p[2], 255])
            .collect(),
        png::ColorType::GrayscaleAlpha => buf
            .chunks(2)
            .flat_map(|p| [p[0], p[0], p[0], p[1]])
            .collect(),
        png::ColorType::Grayscale => buf.iter().flat_map(|&g| [g, g, g, 255]).collect(),
        png::ColorType::Indexed => return None,
    };
    Some(arboard::ImageData {
        width: info.width as usize,
        height: info.height as usize,
        bytes: rgba.into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn png_roundtrip() {
        let image = arboard::ImageData {
            width: 3,
            height: 2,
            bytes: (0..24u8).collect::<Vec<_>>().into(),
        };
        let back = decode_png(&encode_png(&image).unwrap()).unwrap();
        assert_eq!((back.width, back.height), (3, 2));
        assert_eq!(back.bytes, image.bytes);
    }
}
