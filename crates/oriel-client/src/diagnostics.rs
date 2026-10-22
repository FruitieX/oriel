//! Explaining a slow or stuttering stream: what the host reports about the
//! last second plus what the client measures, turned into warnings that
//! stay up while the problem lasts.

use std::time::{Duration, Instant};

use oriel_proto::{Codec, HostHealth};

/// A kind of problem; each shows at most one warning.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Issue {
    Stalled,
    SlowDecode,
    SlowEncode,
    NetworkLimited,
    PacketLoss,
    Latency,
    LateFrames,
}

impl Issue {
    const ALL: [Issue; 7] = [
        Issue::Stalled,
        Issue::SlowDecode,
        Issue::SlowEncode,
        Issue::NetworkLimited,
        Issue::PacketLoss,
        Issue::Latency,
        Issue::LateFrames,
    ];

    /// How long a problem must last before it is shown, so that blips
    /// pass silently.
    fn raise_after(self) -> Duration {
        Duration::from_secs(match self {
            Issue::Stalled => 0,
            Issue::NetworkLimited | Issue::SlowDecode | Issue::SlowEncode => 2,
            Issue::PacketLoss => 3,
            Issue::Latency | Issue::LateFrames => 5,
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
}

/// The problems `inputs` show right now, each with an explanation.
pub fn detect(i: &Inputs) -> Vec<(Issue, String)> {
    let mut out = Vec::new();
    if i.since_pong >= Duration::from_millis(2500) {
        out.push((
            Issue::Stalled,
            format!(
                "No answer from the host for {} s. The network may have dropped; \
                 oriel reconnects if it does not come back.",
                i.since_pong.as_secs()
            ),
        ));
        // Everything else is stale while nothing gets through.
        return out;
    }
    let slow_decode = i.fps >= 5.0 && i.decode_ms * i.fps > 850.0;
    if slow_decode {
        let mut text = format!(
            "Decoding takes {:.0} ms per frame, which limits the stream to about {:.0} fps.",
            i.decode_ms,
            1000.0 / i.decode_ms.max(1.0)
        );
        if i.software_decode {
            text.push_str(
                " Hardware decoding is not in use: try another codec or turn on \
                 hardware decoding in the Video settings.",
            );
        } else {
            text.push_str(" A lower resolution or frame rate helps.");
        }
        out.push((Issue::SlowDecode, text));
    }
    let Some(host) = i.host else {
        return out;
    };
    // The encoder was busy most of the second and still fell behind.
    let slow_encode = host.wanted_fps >= 10
        && host.sent_fps < host.wanted_fps
        && i.encode_ms * host.sent_fps as f32 > 850.0;
    if slow_encode {
        out.push((
            Issue::SlowEncode,
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
        let text = if i.codec == Some(Codec::PyroWave) {
            format!(
                "The network cannot carry PyroWave at {} Mbit/s: {} of {} frames per \
                 second got through. Lower the PyroWave bitrate in the Video settings.",
                i.pyrowave_mbps, host.sent_fps, host.wanted_fps
            )
        } else {
            format!(
                "The network cannot keep up: {} of {} frames per second got through. \
                 Lower the bitrate ({} Mbit/s) or the frame rate in the Video settings.",
                host.sent_fps, host.wanted_fps, i.bitrate_mbps
            )
        };
        out.push((Issue::NetworkLimited, text));
    }
    if host.loss_permille >= 20 {
        out.push((
            Issue::PacketLoss,
            format!(
                "{:.1}% of packets are lost on the way, which causes stutter. Weak \
                 Wi-Fi is the usual cause: a cable or the 5 GHz band helps.",
                host.loss_permille as f32 / 10.0
            ),
        ));
    }
    if i.rtt_ms >= 60.0 {
        out.push((
            Issue::Latency,
            format!(
                "High network latency: {:.0} ms round trip, so input reacts late.",
                i.rtt_ms
            ),
        ));
    } else if i.latency_ms - i.rtt_ms / 2.0 >= 80.0 && out.is_empty() {
        out.push((
            Issue::LateFrames,
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
    texts: [String; Issue::ALL.len()],
}

const KEEP: Duration = Duration::from_secs(4);

impl Diagnostics {
    pub fn update(&mut self, now: Instant, found: Vec<(Issue, String)>) {
        for (n, issue) in Issue::ALL.iter().enumerate() {
            let state = &mut self.states[n];
            if let Some((_, text)) = found.iter().find(|(i, _)| i == issue) {
                let since = *state.since.get_or_insert(now);
                state.seen = Some(now);
                if now.duration_since(since) >= issue.raise_after() {
                    state.shown = true;
                }
                self.texts[n] = text.clone();
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
    pub fn warnings(&self) -> Vec<(Issue, String)> {
        Issue::ALL
            .iter()
            .enumerate()
            .filter(|(n, _)| self.states[*n].shown && !self.states[*n].dismissed)
            .map(|(n, issue)| (*issue, self.texts[n].clone()))
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
        }
    }

    fn issues(i: &Inputs) -> Vec<Issue> {
        detect(i).into_iter().map(|(i, _)| i).collect()
    }

    #[test]
    fn detects_problems() {
        assert!(issues(&healthy()).is_empty());

        let mut i = healthy();
        i.host.as_mut().unwrap().network_limited = true;
        i.host.as_mut().unwrap().sent_fps = 30;
        assert_eq!(issues(&i), [Issue::NetworkLimited]);
        assert!(detect(&i)[0].1.contains("50 Mbit/s"));

        // A slow decoder explains the held-back host by itself.
        i.fps = 30.0;
        i.decode_ms = 32.0;
        i.software_decode = true;
        assert_eq!(issues(&i), [Issue::SlowDecode]);
        assert!(detect(&i)[0].1.contains("Hardware decoding"));

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
    }

    #[test]
    fn warnings_need_to_last() {
        let t0 = Instant::now();
        let mut d = Diagnostics::default();
        let loss = || vec![(Issue::PacketLoss, "loss".to_string())];
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
}
