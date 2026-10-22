//! Text clipboard sync. arboard has no change notifications, so poll.

use std::{sync::mpsc, time::Duration};

/// Max text size synced, to keep the control stream responsive.
pub const MAX_TEXT: usize = 1 << 20;

pub struct ClipboardSync {
    /// Text from the client to put on the host clipboard.
    pub set: mpsc::Sender<String>,
}

/// Start polling; `on_change` gets host-side clipboard changes.
pub fn start(on_change: impl Fn(String) + Send + 'static) -> ClipboardSync {
    let (set_tx, set_rx) = mpsc::channel::<String>();
    std::thread::Builder::new()
        .name("oriel-clipboard".into())
        .spawn(move || {
            let mut clipboard = match arboard::Clipboard::new() {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!("clipboard unavailable: {e}");
                    return;
                }
            };
            let mut last = clipboard.get_text().ok();
            loop {
                match set_rx.recv_timeout(Duration::from_millis(300)) {
                    Ok(text) => {
                        if clipboard.set_text(text.clone()).is_ok() {
                            last = Some(text);
                        }
                        continue;
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => return,
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                }
                let current = clipboard.get_text().ok();
                if current.is_some() && current != last {
                    last = current.clone();
                    if let Some(text) = current.filter(|t| t.len() <= MAX_TEXT) {
                        on_change(text);
                    }
                }
            }
        })
        .expect("spawn clipboard thread");
    ClipboardSync { set: set_tx }
}
