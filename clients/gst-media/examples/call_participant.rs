//! A headless call participant: joins a channel's call through core with a
//! GstEngine, stays until Ctrl-C (or `BROOK_SECONDS`), and reports what it
//! receives. For cross-client interop tests (e.g. macOS <-> Linux) without a
//! GUI.
//!
//! ```sh
//! BROOK_SERVER=http://192.168.1.192:8080 BROOK_HANDLE=... BROOK_PASSWORD=... \
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
    let mic = match std::env::var("BROOK_MIC").as_deref() {
        Ok("auto") => MicSource::Auto,
        Ok("none") => MicSource::None,
        _ => MicSource::Test,
    };
    let silent_output = mic == MicSource::Test;
    let seconds: Option<u64> = std::env::var("BROOK_SECONDS")
        .ok()
        .and_then(|s| s.parse().ok());

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
        Arc::new(move |kind| {
            if kind == TrackKind::Audio && !silent_output {
                return gst::ElementFactory::make("autoaudiosink").build().unwrap();
            }
            let sink = gst::ElementFactory::make("fakesink")
                .property("sync", false)
                .property("signal-handoffs", true)
                .build()
                .unwrap();
            let n = Arc::new(AtomicUsize::new(0));
            let c = n.clone();
            sink.connect("handoff", false, move |_| {
                c.fetch_add(1, Ordering::Relaxed);
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

    let (handle_tx, handle_rx) = oneshot::channel::<Arc<CallHandle>>();
    let tracks: Arc<Mutex<HashMap<String, (TrackKind, String)>>> = Arc::default();
    {
        let tracks = tracks.clone();
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
                    EngineEvent::SubscribeStreams(s) => println!(
                        "subscribe offer applied: {:?}",
                        s.iter()
                            .map(|s| format!("{}={:?}/{}", s.mid, s.kind, s.participant_id))
                            .collect::<Vec<_>>()
                    ),
                    EngineEvent::Error { pc, message } => {
                        println!("ENGINE ERROR {pc:?}: {message}");
                        call.engine_failed(message);
                    }
                    _ => {}
                }
            }
        });
    }

    let engine_dyn: Arc<dyn MediaEngine> = engine;
    let call = client
        .join_call(&channel, engine_dyn, true)
        .await
        .expect("join_call");
    let _ = handle_tx.send(call.clone());
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
                let video: Vec<String> = tracks.lock().unwrap().iter()
                    .filter(|(_, (k, _))| *k == TrackKind::Video)
                    .map(|(mid, (_, sink))| {
                        let n = frames.lock().unwrap().get(sink).map(|n| n.load(Ordering::Relaxed)).unwrap_or(0);
                        format!("mid {mid}: {n} frames")
                    }).collect();
                println!("[{:?}] roster {names:?}; video {video:?}", s.status);
                if matches!(s.status, CallStatus::Ended(_)) {
                    return;
                }
            }
        }
    }
    let _ = call.leave().await;
    println!("left");
}
