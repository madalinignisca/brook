//! A headless call participant: joins a channel's call through core with a
//! GstEngine, stays until Ctrl-C (or `BROOK_SECONDS`), and reports what it
//! receives. For cross-client interop tests (e.g. macOS <-> Linux) without a
//! GUI.
//!
//! ```sh
//! BROOK_SERVER=http://<server>:8080 BROOK_HANDLE=... BROOK_PASSWORD=... \
//! BROOK_CHANNEL=<uuid> [BROOK_CAMERA=test|auto|/dev/videoN] [BROOK_MIC=test|auto|none] \
//! [BROOK_SECONDS=120] cargo run -p brook-media-gst --example call_participant
//! ```
//! Remote audio plays on the default output unless `BROOK_MIC=test`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use brook_core::{BrookClient, CallHandle, CallStatus, CoreConfig, MediaEngine, ServerEvent};
use brook_media_gst::{
    CameraSource, EngineConfig, EngineEvent, GstEngine, MicSource, SinkFactory, TrackKind,
    VideoCodec,
};
use gst::prelude::*;
use tokio::sync::oneshot;

fn var(k: &str) -> String {
    std::env::var(k).unwrap_or_else(|_| panic!("set {k}"))
}

#[tokio::main]
async fn main() {
    let server = var("BROOK_SERVER");
    let channel = var("BROOK_CHANNEL");
    let camera = match std::env::var("BROOK_CAMERA").as_deref() {
        Ok("auto") => CameraSource::Auto,
        Ok("none") => CameraSource::None,
        Ok(p) if p.starts_with('/') => CameraSource::Device(p.into()),
        _ => CameraSource::Test,
    };
    // Echo check (BROOK_ECHO=1): send a speech-like clip as the mic, record
    // what comes back, and report whether the clip returns as echo.
    let echo = std::env::var("BROOK_ECHO").as_deref() == Ok("1");
    let clip = echo.then(|| echo_check::speechlike_clip(CLIP_RATE));
    let mic = match (&clip, std::env::var("BROOK_MIC").as_deref()) {
        (Some(clip), _) => {
            let path = std::env::temp_dir().join("brook-echo-clip.wav");
            echo_check::write_wav(&path, CLIP_RATE, clip).expect("write clip");
            MicSource::File(path)
        }
        (None, Ok("auto")) => MicSource::Auto,
        (None, Ok("none")) => MicSource::None,
        (None, _) => MicSource::Test,
    };
    let silent_output = mic != MicSource::Auto;
    let seconds: Option<u64> = std::env::var("BROOK_SECONDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .or(echo.then_some(echo_check::CLIP_SECONDS + 10));
    let received: echo_check::Received = Arc::default();

    let client = Arc::new(
        BrookClient::new(CoreConfig::with_options(&server, true).unwrap()).expect("client"),
    );
    client
        .login(&var("BROOK_HANDLE"), &var("BROOK_PASSWORD"))
        .await
        .expect("login");
    let mut events = client.events();
    client.start_realtime().await.expect("realtime");
    while !matches!(events.recv().await, Ok(ServerEvent::Ready)) {}

    // Count decoded video frames per sink; audio plays unless muted.
    let frames: Arc<Mutex<HashMap<String, Arc<AtomicUsize>>>> = Arc::default();
    let sink: SinkFactory = {
        let frames = frames.clone();
        let received = received.clone();
        Arc::new(move |kind| {
            if kind == TrackKind::Audio && echo {
                return echo_check::recording_sink(received.clone());
            }
            if kind == TrackKind::Audio && !silent_output {
                return gst::ElementFactory::make("autoaudiosink").build().unwrap();
            }
            let sink = gst::ElementFactory::make("fakesink")
                .property("sync", false)
                .property("signal-handoffs", true)
                .build()
                .unwrap();
            // Video counts decoded frames; audio counts decoded bytes.
            let n = Arc::new(AtomicUsize::new(0));
            let c = n.clone();
            sink.connect("handoff", false, move |args| {
                let step = match kind {
                    TrackKind::Video => 1,
                    TrackKind::Audio => args[1].get::<gst::Buffer>().map_or(0, |b| b.size()),
                };
                c.fetch_add(step, Ordering::Relaxed);
                None
            });
            frames.lock().unwrap().insert(sink.name().to_string(), n);
            sink
        })
    };
    let (engine, mut engine_events) = GstEngine::new(EngineConfig {
        camera,
        mic,
        codec: VideoCodec::H264,
        hardware_encode: std::env::var("BROOK_HW_ENCODE").as_deref() == Ok("1"),
        video_kbps: 1500,
        ice_servers: vec![],
        video_sink: sink.clone(),
        audio_sink: Some(sink),
    })
    .expect("engine");
    let engine_for_echo = engine.clone();

    let (handle_tx, handle_rx) = oneshot::channel::<Arc<CallHandle>>();
    let tracks: Arc<Mutex<HashMap<String, (TrackKind, String)>>> = Arc::default();
    // mid -> participant id, from the latest applied subscribe offer.
    let owners: Arc<Mutex<HashMap<String, String>>> = Arc::default();
    {
        let tracks = tracks.clone();
        let owners = owners.clone();
        tokio::spawn(async move {
            let Ok(call) = handle_rx.await else { return };
            while let Some(ev) = engine_events.recv().await {
                match ev {
                    EngineEvent::LocalCandidate { pc, candidate } => {
                        call.local_candidate(pc, candidate)
                    }
                    EngineEvent::RemoteTrack { mid, kind, sink } => {
                        println!("remote {kind:?} on mid {mid}");
                        tracks
                            .lock()
                            .unwrap()
                            .insert(mid, (kind, sink.name().to_string()));
                    }
                    EngineEvent::ConnectionState { pc, state } => println!("{pc:?} PC: {state:?}"),
                    EngineEvent::SubscribeStreams(s) => {
                        println!(
                            "subscribe offer applied: {:?}",
                            s.iter()
                                .map(|s| format!(
                                    "{}={:?}:{:?}/{}",
                                    s.mid, s.kind, s.source, s.participant_id
                                ))
                                .collect::<Vec<_>>()
                        );
                        owners
                            .lock()
                            .unwrap()
                            .extend(s.iter().map(|s| (s.mid.clone(), s.participant_id.clone())));
                    }
                    EngineEvent::Error { pc, message } => {
                        println!("ENGINE ERROR {pc:?}: {message}");
                        call.engine_failed(message);
                    }
                    _ => {}
                }
            }
        });
    }

    let share_engine = engine.clone();
    let engine_dyn: Arc<dyn MediaEngine> = engine;
    let call = client
        .join_call(&channel, engine_dyn, true)
        .await
        .expect("join_call");
    let _ = handle_tx.send(call.clone());
    // BROOK_SHARE=test: also share a synthetic screen (SMPTE bars) once
    // publishing is up, labelled `screen` through core's labelled offer.
    if std::env::var("BROOK_SHARE").as_deref() == Ok("test") {
        tokio::time::sleep(Duration::from_secs(3)).await;
        match share_engine.start_screen_share(brook_media_gst::ScreenSource::Test) {
            Ok(()) => match call.republish().await {
                Ok(()) => println!("sharing a test-pattern screen"),
                Err(err) => println!("republish for the share failed: {err}"),
            },
            Err(err) => println!("screen share failed: {err}"),
        }
    }
    let mut state = call.state();

    let deadline = seconds.map(|s| tokio::time::Instant::now() + Duration::from_secs(s));
    let mut tick = tokio::time::interval(Duration::from_secs(5));
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            _ = tick.tick() => {
                if deadline.is_some_and(|d| tokio::time::Instant::now() >= d) {
                    break;
                }
                let s = state.borrow_and_update().clone();
                let names: Vec<String> = s.participants.iter().map(|p| {
                    format!("{}(a={},v={})", p.display_name, p.audio, p.video)
                }).collect();
                let count = |sink: &str| frames.lock().unwrap().get(sink).map(|n| n.load(Ordering::Relaxed));
                let (mut video, mut audio) = (Vec::new(), Vec::new());
                for (mid, (kind, sink)) in tracks.lock().unwrap().iter() {
                    match (kind, count(sink)) {
                        (TrackKind::Video, Some(n)) => video.push(format!("mid {mid}: {n} frames")),
                        (TrackKind::Audio, Some(n)) => audio.push(format!("mid {mid}: {n} bytes")),
                        (TrackKind::Audio, None) => audio.push(format!("mid {mid}: playing")),
                        _ => {}
                    }
                }
                println!("[{:?}] roster {names:?}; video {video:?}; audio {audio:?}", s.status);
                if matches!(s.status, CallStatus::Ended(_)) {
                    return;
                }
            }
        }
    }
    if let Some(clip) = &clip {
        // Analyse only the chosen participant's audio (BROOK_ECHO_FROM = display
        // name; default: the only other participant), so a lingering ghost or a
        // third party can't contaminate the measurement.
        let roster = state.borrow().participants.clone();
        let wanted = std::env::var("BROOK_ECHO_FROM").ok();
        let target = match &wanted {
            Some(name) => roster.iter().find(|p| &p.display_name == name),
            None if roster.len() == 1 => roster.first(),
            None => None,
        };
        let chunks: Vec<(u64, Vec<i16>)> = target
            .map(|p| {
                let owners = owners.lock().unwrap();
                let tracks = tracks.lock().unwrap();
                let recorded = received.lock().unwrap();
                owners
                    .iter()
                    .filter(|(_, pid)| **pid == p.participant_id)
                    .filter_map(|(mid, _)| tracks.get(mid))
                    .filter(|(kind, _)| *kind == TrackKind::Audio)
                    .filter_map(|(_, sink)| recorded.get(sink))
                    .flatten()
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        match (target, engine_for_echo.publish_base_time()) {
            (None, _) => println!(
                "echo check: no single target participant (roster {:?}; set BROOK_ECHO_FROM)",
                roster.iter().map(|p| &p.display_name).collect::<Vec<_>>()
            ),
            (Some(_), None) => println!("echo check: no publish pipeline"),
            (Some(p), Some(base)) => println!(
                "{} [from {}]",
                echo_check::report(clip, CLIP_RATE, base.nseconds(), &chunks),
                p.display_name
            ),
        }
    }
    let _ = call.leave().await;
    println!("left");
}

/// Sample rate of the generated echo-check clip.
const CLIP_RATE: u32 = 48_000;

mod echo_check {
    //! The automated echo check: a speech-like clip goes out as our mic; the
    //! remote side's audio comes back decoded; if the remote device plays us
    //! through its speakers and its mic picks that up (no/weak AEC), our clip
    //! reappears in what we receive, delayed. Both pipelines run on the system
    //! clock, so sent and received samples share one timeline.

    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    use gst::prelude::*;

    /// 20 s of silence (baseline), then 120 s of speech-like syllables.
    pub const SILENCE_SECONDS: u64 = 20;
    pub const CLIP_SECONDS: u64 = 140;
    /// Envelope frames: 5 ms.
    const FRAME_MS: u64 = 5;
    const RX_RATE: u64 = 8_000;

    /// Received audio per recording sink (by element name): chunks of
    /// (absolute clock time of the first sample, samples at 8 kHz mono).
    pub type Received = Arc<Mutex<HashMap<String, Vec<(u64, Vec<i16>)>>>>;

    /// Deterministic speech-like signal: voiced syllables (a glottal-ish
    /// harmonic series shaped by moving formants) of 80-300 ms, with gaps and
    /// occasional pauses. Speech-like rather than a tone, since AEC treats
    /// stationary tones differently.
    pub fn speechlike_clip(rate: u32) -> Vec<i16> {
        let rate_f = rate as f64;
        let total = (CLIP_SECONDS * rate as u64) as usize;
        let mut out = vec![0i16; total];
        let mut seed: u64 = 0x5eed_b00c;
        let mut rnd = move || {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((seed >> 33) as f64) / ((1u64 << 31) as f64)
        };
        let mut t = (SILENCE_SECONDS * rate as u64) as usize;
        while t < total {
            let dur = ((0.08 + 0.22 * rnd()) * rate_f) as usize;
            let f0 = 100.0 + 120.0 * rnd();
            let (f1, f2) = (300.0 + 500.0 * rnd(), 900.0 + 1300.0 * rnd());
            let end = (t + dur).min(total);
            for (i, sample) in out[t..end].iter_mut().enumerate() {
                let x = i as f64 / rate_f;
                let env = (std::f64::consts::PI * i as f64 / dur as f64).sin().powi(2);
                let mut v = 0.0;
                for k in 1..=20 {
                    let f = f0 * k as f64;
                    let w = (-((f - f1) / 150.0).powi(2)).exp()
                        + 0.6 * (-((f - f2) / 250.0).powi(2)).exp()
                        + 0.05;
                    v += w * (2.0 * std::f64::consts::PI * f * x).sin() / k as f64;
                }
                *sample = (v * env * 6000.0).clamp(-32000.0, 32000.0) as i16;
            }
            let gap = if rnd() < 0.15 {
                0.4 + 0.4 * rnd()
            } else {
                0.05 + 0.2 * rnd()
            };
            t = end + (gap * rate_f) as usize;
        }
        out
    }

    /// Write mono S16LE PCM as a WAV file.
    pub fn write_wav(path: &std::path::Path, rate: u32, samples: &[i16]) -> std::io::Result<()> {
        let data_len = (samples.len() * 2) as u32;
        let mut bytes = Vec::with_capacity(44 + data_len as usize);
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&(36 + data_len).to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16u32.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes()); // PCM
        bytes.extend_from_slice(&1u16.to_le_bytes()); // mono
        bytes.extend_from_slice(&rate.to_le_bytes());
        bytes.extend_from_slice(&(rate * 2).to_le_bytes());
        bytes.extend_from_slice(&2u16.to_le_bytes());
        bytes.extend_from_slice(&16u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&data_len.to_le_bytes());
        for s in samples {
            bytes.extend_from_slice(&s.to_le_bytes());
        }
        std::fs::write(path, bytes)
    }

    /// A sink that records received audio at 8 kHz mono with absolute times.
    pub fn recording_sink(received: Received) -> gst::Element {
        let bin = gst::parse::bin_from_description(
            "audioconvert ! audioresample ! audio/x-raw,format=S16LE,rate=8000,channels=1 ! \
             fakesink name=rec sync=false signal-handoffs=true",
            true,
        )
        .expect("recording sink");
        let sink = bin.by_name("rec").unwrap();
        let key = bin.name().to_string();
        sink.connect("handoff", false, move |args| {
            let el = args[0].get::<gst::Element>().ok()?;
            let buf = args[1].get::<gst::Buffer>().ok()?;
            let (Some(pts), Some(base)) = (buf.pts(), el.base_time()) else {
                return None;
            };
            let map = buf.map_readable().ok()?;
            let samples = map
                .as_chunks::<2>()
                .0
                .iter()
                .map(|b| i16::from_le_bytes(*b))
                .collect();
            received
                .lock()
                .unwrap()
                .entry(key.clone())
                .or_default()
                .push(((base + pts).nseconds(), samples));
            None
        });
        bin.upcast()
    }

    /// RMS envelope in 5 ms frames of `samples` at `rate`.
    fn envelope(samples: &[i16], rate: u64) -> Vec<f64> {
        let per = (rate * FRAME_MS / 1000) as usize;
        samples
            .chunks(per)
            .map(|c| (c.iter().map(|&s| (s as f64).powi(2)).sum::<f64>() / c.len() as f64).sqrt())
            .collect()
    }

    /// Pearson correlation of a[i] with b[i + lag] over the index range.
    fn pearson(a: &[f64], b: &[f64], range: std::ops::Range<usize>, lag: usize) -> f64 {
        let pairs: Vec<(f64, f64)> = range
            .filter_map(|i| Some((*a.get(i)?, *b.get(i + lag)?)))
            .collect();
        let n = pairs.len() as f64;
        if n < 10.0 {
            return 0.0;
        }
        let (ma, mb) = (
            pairs.iter().map(|p| p.0).sum::<f64>() / n,
            pairs.iter().map(|p| p.1).sum::<f64>() / n,
        );
        let (mut cov, mut va, mut vb) = (0.0, 0.0, 0.0);
        for (x, y) in &pairs {
            cov += (x - ma) * (y - mb);
            va += (x - ma).powi(2);
            vb += (y - mb).powi(2);
        }
        if va == 0.0 || vb == 0.0 {
            0.0
        } else {
            cov / (va * vb).sqrt()
        }
    }

    /// Correlate the received audio with the clip (sent from `base_ns`).
    pub fn report(clip: &[i16], rate: u32, base_ns: u64, received: &[(u64, Vec<i16>)]) -> String {
        let clip_env = envelope(clip, rate as u64);
        let frames = clip_env.len() + 400;
        // Received samples onto the clip's timeline (frame 0 = clip start).
        let per = (RX_RATE * FRAME_MS / 1000) as usize;
        let mut sq = vec![0.0f64; frames];
        let mut cnt = vec![0usize; frames];
        for (t0, chunk) in received {
            let Some(offset) = t0.checked_sub(base_ns) else {
                continue;
            };
            let first = (offset as u128 * RX_RATE as u128 / 1_000_000_000) as usize;
            for (i, &s) in chunk.iter().enumerate() {
                let f = (first + i) / per;
                if f < frames {
                    sq[f] += (s as f64).powi(2);
                    cnt[f] += 1;
                }
            }
        }
        let rx_env: Vec<f64> = sq
            .iter()
            .zip(&cnt)
            .map(|(s, &c)| if c > 0 { (s / c as f64).sqrt() } else { 0.0 })
            .collect();
        let fps = (1000 / FRAME_MS) as usize;
        // The clip's speech runs 20-140 s; 22-138 s keeps syllable edges and
        // join/leave transients out (116 of the 120 s).
        let active = (SILENCE_SECONDS as usize + 2) * fps..(CLIP_SECONDS as usize - 2) * fps;
        let lags = 0..(1500 / FRAME_MS as usize);
        let best = |a: &[f64]| {
            lags.clone()
                .map(|l| (l, pearson(a, &rx_env, active.clone(), l)))
                .fold((0, f64::MIN), |m, x| if x.1 > m.1 { x } else { m })
        };
        let (lag, peak) = best(&clip_env);
        // Chance level: the same correlation against an unrelated part of the clip.
        let shifted: Vec<f64> = clip_env
            .iter()
            .skip(5 * fps)
            .copied()
            .chain(clip_env.iter().take(5 * fps).copied())
            .collect();
        let (_, chance) = best(&shifted);
        let db = |r: std::ops::Range<usize>| {
            let v: Vec<f64> = r
                .filter(|&f| cnt.get(f).is_some_and(|&c| c > 0))
                .map(|f| sq[f] / cnt[f] as f64)
                .collect();
            if v.is_empty() {
                return f64::NAN;
            }
            10.0 * (v.iter().sum::<f64>() / v.len() as f64 / (32768.0f64).powi(2)).log10()
        };
        let silence = 3 * fps..(SILENCE_SECONDS as usize - 1) * fps;
        let received_frames = cnt.iter().filter(|&&c| c > 0).count();
        // The remote side never sends our clip back on purpose: a clear match
        // anywhere in 0-1.5 s means it played us out loud and its mic picked
        // it up. (Timestamps mark capture, so the delay excludes our own
        // jitter buffer.)
        // Correlation ignores scale, so also require the clip to raise the
        // received level: an AEC that suppresses echo leaves a correlated
        // residual that barely lifts the level above the room's.
        let (silent_db, playing_db) = (db(silence.clone()), db(active.clone()));
        let lift = playing_db - silent_db; // +inf if the silent window was digital silence
        let verdict = match (peak > (3.0 * chance).max(0.2), lift >= 6.0) {
            (true, true) => "ECHO: our clip comes back",
            (true, false) => "residual echo only (correlated, but < 6 dB above the silent window)",
            (false, _) => "no echo detected",
        };
        format!(
 "echo check: {verdict}: peak envelope correlation {peak:.3} at {} ms (chance level {chance:.3}); received level {:.1} dBFS while our clip was silent vs {:.1} dBFS while it played; {received_frames} frames of received audio",
            lag * FRAME_MS as usize,
            silent_db,
            playing_db
        )
    }
}
