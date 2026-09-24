//! A real call through core's signaling (`BrookClient::join_call` +
//! `CallHandle`) with two GStreamer engines against a live Brook server +
//! Janus: the Linux client stack end to end, minus the UI. This is C1b's live
//! acceptance on Linux.
//!
//! Ignored by default; same environment as `janus_interop`:
//!
//! ```sh
//! BROOK_TEST_SERVER=http://localhost:8080 BROOK_TEST_CHANNEL=<uuid> \
//! BROOK_TEST_USERS=alice:pw,bob:pw \
//!   cargo test -p brook-media-gst --test core_call -- --ignored --nocapture
//! ```

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use brook_core::{
    BrookClient, CallHandle, CallState, CallStatus, CoreConfig, EndReason, MediaKind, ServerEvent,
    SubStream,
};
use brook_media_gst::{
    CameraSource, EngineConfig, EngineEvent, GstEngine, MicSource, SinkFactory, TrackKind,
    VideoCodec,
};
use gst::prelude::*;
use tokio::sync::{oneshot, watch};

const TIMEOUT: Duration = Duration::from_secs(20);

struct Member {
    name: String,
    handle: Arc<CallHandle>,
    state: watch::Receiver<CallState>,
    /// Decoded video frames per subscribe mid.
    video: Arc<Mutex<HashMap<String, Arc<AtomicUsize>>>>,
    /// The latest applied subscribe offer's streams.
    streams: Arc<Mutex<Vec<SubStream>>>,
    _client: Arc<BrookClient>,
}

async fn join(server: &str, channel: &str, handle: &str, password: &str) -> Member {
    let client =
        Arc::new(BrookClient::new(CoreConfig::with_options(server, true).unwrap()).unwrap());
    client.login(handle, password).await.unwrap();
    let mut events = client.events();
    client.start_realtime().await.unwrap();
    tokio::time::timeout(TIMEOUT, async {
        loop {
            if let Ok(ServerEvent::Ready) = events.recv().await {
                break;
            }
        }
    })
    .await
    .expect("ws ready");

    // Engine whose video sinks count frames, keyed by sink name until the
    // RemoteTrack event says which mid they belong to.
    let by_sink: Arc<Mutex<HashMap<String, Arc<AtomicUsize>>>> = Arc::default();
    let sink: SinkFactory = {
        let by_sink = by_sink.clone();
        Arc::new(move |kind| {
            let sink = gst::ElementFactory::make("fakesink")
                .property("sync", false)
                .property("signal-handoffs", kind == TrackKind::Video)
                .build()
                .unwrap();
            if kind == TrackKind::Video {
                let n = Arc::new(AtomicUsize::new(0));
                let c = n.clone();
                sink.connect("handoff", false, move |_| {
                    c.fetch_add(1, Ordering::Relaxed);
                    None
                });
                by_sink.lock().unwrap().insert(sink.name().to_string(), n);
            }
            sink
        })
    };
    let (engine, mut engine_events) = GstEngine::new(EngineConfig {
        camera: CameraSource::Test,
        mic: MicSource::Test,
        codec: VideoCodec::H264,
        hardware_encode: false,
        video_kbps: 1000,
        ice_servers: vec![],
        video_sink: sink.clone(),
        audio_sink: Some(sink),
    })
    .unwrap();

    // Engine -> core, as a client's glue does it. Events that arrive before
    // join_call returns (publish candidates) wait in the channel.
    let (handle_tx, handle_rx) = oneshot::channel::<Arc<CallHandle>>();
    let video: Arc<Mutex<HashMap<String, Arc<AtomicUsize>>>> = Arc::default();
    let streams: Arc<Mutex<Vec<SubStream>>> = Arc::default();
    {
        let (video, streams, name) = (video.clone(), streams.clone(), handle.to_string());
        tokio::spawn(async move {
            let Ok(call) = handle_rx.await else { return };
            while let Some(ev) = engine_events.recv().await {
                match ev {
                    EngineEvent::LocalCandidate { pc, candidate } => {
                        call.local_candidate(pc, candidate)
                    }
                    EngineEvent::SubscribeStreams(s) => *streams.lock().unwrap() = s,
                    EngineEvent::RemoteTrack { mid, kind, sink } => {
                        println!("[{name}] remote {kind:?} on mid {mid}");
                        if let Some(n) = by_sink.lock().unwrap().get(sink.name().as_str()) {
                            video.lock().unwrap().insert(mid, n.clone());
                        }
                    }
                    EngineEvent::Error { pc, message } => {
                        println!("[{name}] engine error {pc:?}: {message}");
                        call.engine_failed(message);
                    }
                    _ => {}
                }
            }
        });
    }

    let call = tokio::time::timeout(TIMEOUT, client.join_call(channel, engine, true))
        .await
        .expect("join_call in time")
        .expect("join_call");
    let _ = handle_tx.send(call.clone());
    let state = call.state();
    println!(
        "[{handle}] joined: {:?} as {:?} in {:?}",
        state.borrow().status,
        state.borrow().self_participant,
        state.borrow().call_id
    );
    Member {
        name: handle.to_string(),
        handle: call,
        state,
        video,
        streams,
        _client: client,
    }
}

impl Member {
    /// Wait until `pred` holds for the call state.
    async fn until(&mut self, what: &str, pred: impl Fn(&CallState) -> bool) {
        let ok = tokio::time::timeout(TIMEOUT, self.state.wait_for(|s| pred(s)))
            .await
            .is_ok_and(|r| r.is_ok());
        assert!(
            ok,
            "[{}] timed out waiting for {what}: {:?}",
            self.name,
            self.state.borrow()
        );
    }

    /// Wait until a remote video stream of the latest offer decodes `frames` more.
    async fn decodes_video(&self, frames: usize) {
        let base: HashMap<String, usize> = self.counts();
        let deadline = tokio::time::Instant::now() + TIMEOUT;
        loop {
            let now = self.counts();
            let active: Vec<String> = self
                .streams
                .lock()
                .unwrap()
                .iter()
                .filter(|s| s.kind == MediaKind::Video)
                .map(|s| s.mid.clone())
                .collect();
            if active.iter().any(|m| {
                now.get(m).copied().unwrap_or(0) >= base.get(m).copied().unwrap_or(0) + frames
            }) {
                println!("[{}] decoding remote video {now:?}", self.name);
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "[{}] no remote video: {now:?}, active {active:?}",
                self.name
            );
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    fn counts(&self) -> HashMap<String, usize> {
        self.video
            .lock()
            .unwrap()
            .iter()
            .map(|(m, n)| (m.clone(), n.load(Ordering::Relaxed)))
            .collect()
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a running Brook server + Janus (see module docs)"]
async fn call_through_core() {
    let var = |k: &str| std::env::var(k).unwrap_or_else(|_| panic!("set {k}"));
    let (server, channel) = (var("BROOK_TEST_SERVER"), var("BROOK_TEST_CHANNEL"));
    let users: Vec<(String, String)> = var("BROOK_TEST_USERS")
        .split(',')
        .map(|u| {
            let (h, p) = u.split_once(':').unwrap();
            (h.to_string(), p.to_string())
        })
        .collect();

    let mut alice = join(&server, &channel, &users[0].0, &users[0].1).await;
    alice
        .until("Connected", |s| s.status == CallStatus::Connected)
        .await;
    let mut bob = join(&server, &channel, &users[1].0, &users[1].1).await;
    bob.until("alice in roster", |s| s.participants.len() == 1)
        .await;
    alice
        .until("bob in roster", |s| s.participants.len() == 1)
        .await;

    // Media both ways, through core-driven negotiation.
    alice.decodes_video(30).await;
    bob.decodes_video(30).await;

    // Bob turns his camera off: alice's roster reflects it (call.media).
    bob.handle.set_media(true, false).await.unwrap();
    alice
        .until("bob video off", |s| {
            s.participants.first().is_some_and(|p| !p.video)
        })
        .await;
    bob.handle.set_media(true, true).await.unwrap();
    alice
        .until("bob video on", |s| {
            s.participants.first().is_some_and(|p| p.video)
        })
        .await;

    // Bob leaves: his status ends Left; alice's roster empties and her
    // subscribe PC renegotiates to no streams while she stays connected.
    bob.handle.leave().await.unwrap();
    bob.until("Ended(Left)", |s| {
        s.status == CallStatus::Ended(EndReason::Left)
    })
    .await;
    alice
        .until("empty roster", |s| s.participants.is_empty())
        .await;
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    while !alice.streams.lock().unwrap().is_empty() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "alice still has streams"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(alice.state.borrow().status, CallStatus::Connected);

    alice.handle.leave().await.unwrap();
    alice
        .until("Ended(Left)", |s| {
            s.status == CallStatus::Ended(EndReason::Left)
        })
        .await;
}
