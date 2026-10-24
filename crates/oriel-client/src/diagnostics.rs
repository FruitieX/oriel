//! Explaining a slow or stuttering stream: what the host reports about the
//! last second plus what the client measures, turned into warnings that
//! stay up while the problem lasts.

use std::time::{Duration, Instant};

use oriel_proto::{Codec, HostHealth};

/// A kind of problem; each shows at most one warning.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Issue {
    Version,
    Stalled,
    SlowDecode,
    SoftwareDecode,
    SoftwareEncode,
    SlowEncode,
    NetworkLimited,
    PacketLoss,
    Latency,
    Jitter,
    AudioDropouts,
    LateFrames,
}

impl Issue {
    const ALL: [Issue; 12] = [
        Issue::Version,
        Issue::Stalled,
        Issue::SlowDecode,
        Issue::SoftwareDecode,
        Issue::SoftwareEncode,
        Issue::SlowEncode,
        Issue::NetworkLimited,
        Issue::PacketLoss,
        Issue::Latency,
        Issue::Jitter,
        Issue::AudioDropouts,
        Issue::LateFrames,
    ];

    /// How long a problem must last before it is shown, so that blips
    /// pass silently.
    fn raise_after(self) -> Duration {
        Duration::from_secs(match self {
            Issue::Stalled | Issue::Version => 0,
            Issue::NetworkLimited
            | Issue::SlowDecode
            | Issue::SoftwareDecode
            | Issue::SoftwareEncode
            | Issue::SlowEncode => 2,
            Issue::PacketLoss | Issue::AudioDropouts => 3,
            Issue::Latency | Issue::Jitter | Issue::LateFrames => 5,
        })
    }
}

/// The measurements one check looks at.
#[derive(Debug, Clone, Default)]
pub struct Inputs {
    /// The host's last report, if recent.
    pub host: Option<HostHealth>,
    /// Since the host last answered a ping.
    pub since_pong: Duration,
    pub rtt_ms: f32,
    /// Frames shown in the last second and their average decode time.
    pub fps: f32,
    pub decode_ms: f32,
    pub encode_ms: f32,
    /// Host capture to client screen.
    pub latency_ms: f32,
    pub codec: Option<Codec>,
    pub bitrate_mbps: u32,
    pub pyrowave_mbps: u32,
    pub software_decode: bool,
    /// Hardware decoding and encoding are chosen (rather than software).
    pub hardware_decode: bool,
    pub hardware_encode: bool,
    /// The host's encoder, e.g. "hevc_nvenc".
    pub encoder: Option<String>,
    /// The stream has full-resolution colour (4:4:4).
    pub full_chroma: bool,
    /// The host lowers the bitrate by itself.
    pub auto_bitrate: bool,
    /// The host's protocol version, when it is not ours.
    pub other_protocol: Option<u32>,
    /// Variation in frames' network delay.
    pub jitter_ms: f32,
    /// The smoothing buffer (`None`: off), and the share of frames that
    /// came too late for it, 0–1.
    pub buffer_ms: Option<f32>,
    pub late: f32,
    /// Times a second the sound ran dry.
    pub audio_underruns: f32,
}

/// A problem seen right now.
#[derive(Debug, Clone, PartialEq)]
pub struct Found {
    pub issue: Issue,
    /// The problem and its numbers.
    pub short: String,
    /// The same with an explanation and what to try.
    pub detail: String,
}

fn found(issue: Issue, short: String, detail: String) -> Found {
    Found {
        issue,
        short,
        detail,
    }
}

/// The problems `inputs` show right now.
pub fn detect(i: &Inputs) -> Vec<Found> {
    let mut out = Vec::new();
    if let Some(host) = i.other_protocol {
        let ours = oriel_proto::PROTOCOL_VERSION;
        out.push(found(
            Issue::Version,
            format!("Different oriel versions (host {host}, this device {ours})"),
            format!(
                "The host runs another version of oriel (protocol {host}; this device \
                 {ours}). What both understand works; update both for the rest."
            ),
        ));
    }
    if i.since_pong >= Duration::from_millis(2500) {
        let secs = i.since_pong.as_secs();
        out.push(found(
            Issue::Stalled,
            format!("No answer from the host for {secs} s"),
            format!(
                "No answer from the host for {secs} s. The network may have dropped; \
                 oriel reconnects if it does not come back."
            ),
        ));
        // Everything else is stale while nothing gets through.
        return out;
    }
    let slow_decode = i.fps >= 5.0 && i.decode_ms * i.fps > 850.0;
    if slow_decode {
        let mut detail = format!(
            "Decoding takes {:.0} ms per frame, which limits the stream to about {:.0} fps.",
            i.decode_ms,
            1000.0 / i.decode_ms.max(1.0)
        );
        if i.software_decode && !i.hardware_decode {
            detail.push_str(
                " Decoding is set to software in the Video settings; hardware is \
                 faster.",
            );
        } else if i.software_decode && i.full_chroma {
            detail.push_str(
                " This computer decodes 4:4:4 in software: turn off Colour detail in \
                 the Video settings.",
            );
        } else if i.software_decode {
            detail.push_str(" This computer's GPU cannot decode it: try another codec.");
        } else {
            detail.push_str(" A lower resolution or frame rate helps.");
        }
        let software = if i.software_decode { " (software)" } else { "" };
        out.push(found(
            Issue::SlowDecode,
            format!("Slow decoding{software}: {:.0} ms per frame", i.decode_ms),
            detail,
        ));
    }
    // Hardware was chosen but is not what runs. Said even while fast
    // enough: it costs CPU and latency all the same.
    if i.software_decode && i.hardware_decode && !slow_decode && i.fps > 0.0 {
        let (title, detail) = if i.full_chroma {
            (
                "4:4:4 decoded in software",
                "This computer's GPU cannot decode 4:4:4 video, so the CPU does it, with \
                 more latency. Turn off Colour detail in the Video settings to decode in \
                 hardware.",
            )
        } else {
            (
                "Decoding in software",
                "This computer's GPU cannot decode this stream, so the CPU does it, with \
                 more latency. Another codec may decode in hardware.",
            )
        };
        out.push(found(Issue::SoftwareDecode, title.into(), detail.into()));
    }
    if i.hardware_encode
        && i.fps > 0.0
        && let Some(encoder) = i.encoder.as_deref().filter(|e| e.starts_with("lib"))
    {
        out.push(found(
            Issue::SoftwareEncode,
            format!("The host encodes in software ({encoder})"),
            "The host's GPU cannot encode this stream, so its CPU does, with more \
             latency. Another codec (or turning off Colour detail) may encode in \
             hardware."
                .into(),
        ));
    }
    let Some(host) = i.host else {
        return out;
    };
    // The encoder was busy most of the second and still fell behind.
    let slow_encode = host.wanted_fps >= 10
        && host.sent_fps < host.wanted_fps
        && i.encode_ms * host.sent_fps as f32 > 850.0;
    if slow_encode {
        out.push(found(
            Issue::SlowEncode,
            format!("Slow encoding on the host: {:.0} ms per frame", i.encode_ms),
            format!(
                "The host needs {:.0} ms to encode a frame, too slow for {} fps. \
                 A lower resolution or frame rate helps.",
                i.encode_ms, host.wanted_fps
            ),
        ));
    }
    // A client that decodes slowly holds the host back the same way a slow
    // link does; then the decoder is the one to blame.
    if host.network_limited && !slow_decode && !slow_encode {
        let short = format!(
            "Network too slow: {} of {} fps sent",
            host.sent_fps, host.wanted_fps
        );
        let detail = if i.codec == Some(Codec::PyroWave) {
            format!(
                "The network cannot carry PyroWave at {} Mbit/s: {} of {} frames per \
                 second got through. Lower the PyroWave bitrate in the Video settings.",
                i.pyrowave_mbps, host.sent_fps, host.wanted_fps
            )
        } else if i.auto_bitrate && host.bitrate_kbps > 0 {
            format!(
                "The network cannot keep up even with the bitrate lowered to {:.1} Mbit/s: \
                 {} of {} frames per second got through. A lower frame rate in the Video \
                 settings helps.",
                host.bitrate_kbps as f32 / 1000.0,
                host.sent_fps,
                host.wanted_fps
            )
        } else {
            format!(
                "The network cannot keep up: {} of {} frames per second got through. \
                 Lower the bitrate ({} Mbit/s) or the frame rate in the Video settings, \
                 or turn on automatic bitrate.",
                host.sent_fps, host.wanted_fps, i.bitrate_mbps
            )
        };
        out.push(found(Issue::NetworkLimited, short, detail));
    }
    if host.loss_permille >= 20 {
        let pct = host.loss_permille as f32 / 10.0;
        out.push(found(
            Issue::PacketLoss,
            format!("Packet loss: {pct:.1}%"),
            format!(
                "{pct:.1}% of packets are lost on the way, which causes stutter. Weak \
                 Wi-Fi is the usual cause: a cable or the 5 GHz band helps."
            ),
        ));
    }
    match i.buffer_ms {
        None if i.jitter_ms >= 10.0 && i.fps >= 10.0 => out.push(found(
            Issue::Jitter,
            format!("Network jitter: {:.0} ms", i.jitter_ms),
            format!(
                "Frames' network delay varies by about {:.0} ms (jitter), so motion is \
                 uneven. Smoothness in the Video settings evens it out for a little \
                 latency; a cable instead of Wi-Fi helps too.",
                i.jitter_ms
            ),
        )),
        // The buffer absorbs jitter unless frames keep missing it.
        Some(buffer) if i.late >= 0.05 && i.fps >= 10.0 => out.push(found(
            Issue::Jitter,
            format!("{:.0}% of frames late for the buffer", i.late * 100.0),
            format!(
                "{:.0}% of frames arrive later than the {buffer:.0} ms smoothing buffer \
                 allows (jitter {:.0} ms). A larger Smoothness setting evens them out.",
                i.late * 100.0,
                i.jitter_ms
            ),
        )),
        _ => {}
    }
    if i.audio_underruns >= 2.0 {
        out.push(found(
            Issue::AudioDropouts,
            format!("Sound drops out ({:.0}× a second)", i.audio_underruns),
            format!(
                "The sound runs dry about {:.0} times a second because its packets \
                 arrive late or not at all, usually from network jitter or loss.",
                i.audio_underruns
            ),
        ));
    }
    if i.rtt_ms >= 60.0 {
        out.push(found(
            Issue::Latency,
            format!("High latency: {:.0} ms round trip", i.rtt_ms),
            format!(
                "High network latency: {:.0} ms round trip, so input reacts late.",
                i.rtt_ms
            ),
        ));
    } else if i.latency_ms - i.rtt_ms / 2.0 >= 80.0 && out.is_empty() {
        out.push(found(
            Issue::LateFrames,
            format!("Frames {:.0} ms late", i.latency_ms),
            format!(
                "Frames reach the screen {:.0} ms after the host captured them, \
                 although the network is quick. A lower frame rate or bitrate may help.",
                i.latency_ms
            ),
        ));
    }
    out
}

#[derive(Default, Clone, Copy)]
struct State {
    /// Since when the problem has been seen without a break.
    since: Option<Instant>,
    /// Last seen.
    seen: Option<Instant>,
    shown: bool,
    dismissed: bool,
}

/// Warnings with hysteresis: shown once a problem lasts, kept a few
/// seconds after it ends, and hidden for good when dismissed (until the
/// problem goes away and comes back).
#[derive(Default)]
pub struct Diagnostics {
    states: [State; Issue::ALL.len()],
    found: [Option<Found>; Issue::ALL.len()],
}

const KEEP: Duration = Duration::from_secs(4);

impl Diagnostics {
    pub fn update(&mut self, now: Instant, found: Vec<Found>) {
        for (n, issue) in Issue::ALL.iter().enumerate() {
            let state = &mut self.states[n];
            if let Some(f) = found.iter().find(|f| f.issue == *issue) {
                let since = *state.since.get_or_insert(now);
                state.seen = Some(now);
                if now.duration_since(since) >= issue.raise_after() {
                    state.shown = true;
                }
                self.found[n] = Some(f.clone());
            } else {
                state.since = None;
                if state
                    .seen
                    .is_none_or(|seen| now.duration_since(seen) >= KEEP)
                {
                    *state = State::default();
                }
            }
        }
    }

    pub fn dismiss(&mut self, issue: Issue) {
        if let Some(n) = Issue::ALL.iter().position(|i| *i == issue) {
            self.states[n].dismissed = true;
        }
    }

    /// The warnings to show, most important first.
    pub fn warnings(&self) -> Vec<Found> {
        Issue::ALL
            .iter()
            .enumerate()
            .filter(|(n, _)| self.states[*n].shown && !self.states[*n].dismissed)
            .filter_map(|(n, _)| self.found[n].clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn healthy() -> Inputs {
        Inputs {
            host: Some(HostHealth {
                sent_fps: 60,
                wanted_fps: 60,
                network_limited: false,
                loss_permille: 0,
                bitrate_kbps: 50_000,
            }),
            since_pong: Duration::from_millis(300),
            rtt_ms: 3.0,
            fps: 60.0,
            decode_ms: 2.0,
            encode_ms: 3.0,
            latency_ms: 20.0,
            codec: Some(Codec::Hevc),
            bitrate_mbps: 50,
            pyrowave_mbps: 400,
            software_decode: false,
            hardware_decode: true,
            hardware_encode: true,
            encoder: Some("hevc_nvenc".into()),
            full_chroma: false,
            auto_bitrate: false,
            other_protocol: None,
            jitter_ms: 1.0,
            buffer_ms: None,
            late: 0.0,
            audio_underruns: 0.0,
        }
    }

    fn issues(i: &Inputs) -> Vec<Issue> {
        detect(i).into_iter().map(|f| f.issue).collect()
    }

    #[test]
    fn detects_problems() {
        assert!(issues(&healthy()).is_empty());

        let mut i = healthy();
        i.host.as_mut().unwrap().network_limited = true;
        i.host.as_mut().unwrap().sent_fps = 30;
        assert_eq!(issues(&i), [Issue::NetworkLimited]);
        assert!(detect(&i)[0].detail.contains("50 Mbit/s"));

        // A slow decoder explains the held-back host by itself.
        i.fps = 30.0;
        i.decode_ms = 32.0;
        i.software_decode = true;
        assert_eq!(issues(&i), [Issue::SlowDecode]);
        assert!(detect(&i)[0].detail.contains("GPU cannot decode"));

        let mut i = healthy();
        i.host.as_mut().unwrap().loss_permille = 45;
        i.rtt_ms = 80.0;
        assert_eq!(issues(&i), [Issue::PacketLoss, Issue::Latency]);

        // Held back by the network, not by an encoder that has time left.
        let mut i = healthy();
        let h = i.host.as_mut().unwrap();
        (h.network_limited, h.sent_fps, h.wanted_fps) = (true, 60, 120);
        i.encode_ms = 8.0;
        assert_eq!(issues(&i), [Issue::NetworkLimited]);
        i.encode_ms = 15.0;
        assert_eq!(issues(&i), [Issue::SlowEncode]);

        let mut i = healthy();
        i.since_pong = Duration::from_secs(4);
        i.rtt_ms = 200.0;
        assert_eq!(issues(&i), [Issue::Stalled]);

        let mut i = healthy();
        i.latency_ms = 150.0;
        assert_eq!(issues(&i), [Issue::LateFrames]);

        let mut i = healthy();
        i.jitter_ms = 15.0;
        i.audio_underruns = 4.0;
        i.other_protocol = Some(oriel_proto::PROTOCOL_VERSION - 1);
        assert_eq!(
            issues(&i),
            [Issue::Version, Issue::Jitter, Issue::AudioDropouts]
        );

        // A smoothing buffer that holds: no jitter warning; one that does
        // not: a warning about late frames.
        let mut i = healthy();
        i.jitter_ms = 15.0;
        i.buffer_ms = Some(40.0);
        assert!(issues(&i).is_empty());
        i.late = 0.2;
        assert_eq!(issues(&i), [Issue::Jitter]);
        assert!(detect(&i)[0].detail.contains("40 ms"));
    }

    #[test]
    fn warnings_need_to_last() {
        let t0 = Instant::now();
        let mut d = Diagnostics::default();
        let loss = || vec![found(Issue::PacketLoss, "loss".into(), "loss".into())];
        d.update(t0, loss());
        assert!(d.warnings().is_empty(), "a blip is not shown");
        d.update(t0 + Duration::from_secs(3), loss());
        assert_eq!(d.warnings().len(), 1);
        // Kept briefly after it ends, then gone.
        d.update(t0 + Duration::from_secs(4), vec![]);
        assert_eq!(d.warnings().len(), 1);
        d.update(t0 + Duration::from_secs(8), vec![]);
        assert!(d.warnings().is_empty());

        // Dismissed stays hidden while it lasts.
        d.update(t0 + Duration::from_secs(10), loss());
        d.update(t0 + Duration::from_secs(13), loss());
        d.dismiss(Issue::PacketLoss);
        d.update(t0 + Duration::from_secs(14), loss());
        assert!(d.warnings().is_empty());
    }

    #[test]
    fn software_coding_is_said_only_when_hardware_was_chosen() {
        let mut i = healthy();
        i.encoder = Some("libx265".into());
        assert_eq!(detect(&i)[0].issue, Issue::SoftwareEncode);
        i.hardware_encode = false;
        assert!(detect(&i).is_empty());
        i.software_decode = true;
        assert_eq!(detect(&i)[0].issue, Issue::SoftwareDecode);
        i.hardware_decode = false;
        assert!(detect(&i).is_empty());
    }

    #[test]
    fn software_full_chroma_is_said_even_when_fast_enough() {
        let mut i = healthy();
        i.full_chroma = true;
        assert!(detect(&i).is_empty());
        i.software_decode = true;
        let found = detect(&i);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].issue, Issue::SoftwareDecode);
        // Too slow: the slow decoding warning says what to change instead.
        i.decode_ms = 20.0;
        let found = detect(&i);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].issue, Issue::SlowDecode);
        assert!(found[0].detail.contains("Colour detail"));
    }
}
