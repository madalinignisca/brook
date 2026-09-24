//! Interop against a real Brook server + Janus (PROTOCOL.md §3), with a
//! minimal in-test signaling client. NOT the product's signaling (that is
//! core's `CallHandle`); this exists to prove webrtcbin ⇄ Janus VideoRoom
//! behaviour, especially multistream re-offers on the one subscribe PC.
//!
//! Ignored by default. Run against a stack with two members of one channel:
//!
//! ```sh
//! BROOK_TEST_SERVER=http://localhost:8080 BROOK_TEST_CHANNEL=<uuid> \
//! BROOK_TEST_USERS=alice:pw,bob:pw \
//!   cargo test -p brook-media-gst --test janus_interop -- --ignored --nocapture
//! ```

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use brook_media_gst::{
    CameraSource, EngineConfig, EngineEvent, GstEngine, IceCandidate, MicSource, PcKind,
    SinkFactory, TrackKind, VideoCodec,
};
use futures_util::{SinkExt, StreamExt};
use gst::prelude::*;
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message as Ws;

const TIMEOUT: Duration = Duration::from_secs(20);

struct Env {
    server: String,
    channel: String,
    users: Vec<(String, String)>,
}

fn env() -> Env {
    let var = |k: &str| std::env::var(k).unwrap_or_else(|_| panic!("set {k}"));
    Env {
        server: var("BROOK_TEST_SERVER"),
        channel: var("BROOK_TEST_CHANNEL"),
        users: var("BROOK_TEST_USERS")
            .split(',')
            .map(|u| {
                let (h, p) = u.split_once(':').expect("handle:password");
                (h.to_string(), p.to_string())
            })
            .collect(),
    }
}

/// Counts decoded video buffers per subscribe-PC mid.
#[derive(Default)]
struct Counters {
    video: Mutex<HashMap<String, Arc<AtomicUsize>>>,
}

/// One call participant: WS signaling + its own engine.
struct Participant {
    name: String,
    engine: Arc<GstEngine>,
    cmd: mpsc::UnboundedSender<(Value, Option<oneshot::Sender<Value>>)>,
    call_id: String,
    counters: Arc<Counters>,
    /// mid -> participant_id from the latest subscribe offer.
    streams: Arc<Mutex<HashMap<String, (String, String)>>>,
    /// Latest subscribe version answered.
    answered: Arc<AtomicUsize>,
}

impl Participant {
    async fn join(env: &Env, handle: &str, password: &str) -> Self {
        // REST login.
        let http = reqwest::Client::new();
        let login: Value = http
            .post(format!("{}/api/v1/auth/login", env.server))
            .json(&json!({"handle": handle, "password": password}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let token = login["access_token"]
            .as_str()
            .expect("access_token")
            .to_string();

        // WS + auth.
        let ws_url = format!("{}/ws", env.server.replacen("http", "ws", 1));
        let (socket, _) = tokio_tungstenite::connect_async(ws_url).await.unwrap();
        let (mut tx, mut rx) = socket.split();
        tx.send(Ws::Text(
            json!({"type": "auth", "data": {"access_token": token}}).to_string(),
        ))
        .await
        .unwrap();

        // Engine with counting video sinks, keyed by sink name until the
        // RemoteTrack event tells us the sink's mid.
        let counters = Arc::new(Counters::default());
        let pending: Arc<Mutex<HashMap<String, Arc<AtomicUsize>>>> = Arc::default();
        let sink: SinkFactory = {
            let pending = pending.clone();
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
                    pending.lock().unwrap().insert(sink.name().to_string(), n);
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

        // Outgoing command pump with `re` correlation; incoming dispatch.
        let (cmd_tx, mut cmd_rx) =
            mpsc::unbounded_channel::<(Value, Option<oneshot::Sender<Value>>)>();
        let waiting: Arc<Mutex<HashMap<String, oneshot::Sender<Value>>>> = Arc::default();
        let (event_tx, mut event_rx) = mpsc::unbounded_channel::<Value>();
        {
            let waiting = waiting.clone();
            tokio::spawn(async move {
                let mut n = 0u64;
                while let Some((mut frame, reply)) = cmd_rx.recv().await {
                    n += 1;
                    let id = format!("c{n}");
                    frame["id"] = json!(id);
                    if let Some(reply) = reply {
                        waiting.lock().unwrap().insert(id, reply);
                    }
                    if tx.send(Ws::Text(frame.to_string())).await.is_err() {
                        break;
                    }
                }
            });
        }
        {
            let waiting = waiting.clone();
            tokio::spawn(async move {
                while let Some(Ok(msg)) = rx.next().await {
                    let Ws::Text(text) = msg else { continue };
                    let frame: Value = serde_json::from_str(&text).unwrap();
                    if let Some(re) = frame["re"].as_str() {
                        if let Some(w) = waiting.lock().unwrap().remove(re) {
                            let _ = w.send(frame);
                            continue;
                        }
                    }
                    let _ = event_tx.send(frame);
                }
            });
        }
        // Wait for `ready`.
        loop {
            let f = tokio::time::timeout(TIMEOUT, event_rx.recv())
                .await
                .unwrap()
                .unwrap();
            if f["type"] == "ready" {
                break;
            }
        }

        let request = |cmd: &mpsc::UnboundedSender<_>, frame: Value| {
            let (tx, rx) = oneshot::channel();
            cmd.send((frame, Some(tx))).unwrap();
            async move {
                tokio::time::timeout(TIMEOUT, rx)
                    .await
                    .expect("reply")
                    .unwrap()
            }
        };

        let joined = request(
            &cmd_tx,
            json!({"type": "call.join", "data": {"channel_id": env.channel}}),
        )
        .await;
        assert_eq!(joined["type"], "call.joined", "{handle}: {joined}");
        let call_id = joined["data"]["call_id"].as_str().unwrap().to_string();
        println!("[{handle}] joined call {call_id}");

        let me = Participant {
            name: handle.to_string(),
            engine: engine.clone(),
            cmd: cmd_tx.clone(),
            call_id: call_id.clone(),
            counters: counters.clone(),
            streams: Arc::default(),
            answered: Arc::default(),
        };

        // Engine events -> call.ice; remote tracks -> counters by mid.
        {
            let cmd = cmd_tx.clone();
            let call_id = call_id.clone();
            let counters = counters.clone();
            let pending = pending.clone();
            let name = handle.to_string();
            tokio::spawn(async move {
                while let Some(ev) = engine_events.recv().await {
                    match ev {
                        EngineEvent::LocalCandidate { pc, candidate } => {
                            let pc = match pc {
                                PcKind::Publish => "publish",
                                PcKind::Subscribe => "subscribe",
                            };
                            let candidate = candidate.map(|c| {
                                json!({"candidate": c.candidate, "sdpMid": c.sdp_mid,
                                       "sdpMLineIndex": c.sdp_mline_index})
                            });
                            let _ = cmd.send((
                                json!({"type": "call.ice", "data": {"call_id": call_id,
                                        "pc": pc, "candidate": candidate}}),
                                None,
                            ));
                        }
                        EngineEvent::RemoteTrack { mid, kind, sink } => {
                            println!("[{name}] remote {kind:?} on mid {mid}");
                            if kind == TrackKind::Video {
                                let n = pending.lock().unwrap().get(sink.name().as_str()).cloned();
                                if let Some(n) = n {
                                    counters.video.lock().unwrap().insert(mid, n);
                                }
                            }
                        }
                        EngineEvent::ConnectionState { pc, state } => {
                            println!("[{name}] {pc:?} PC {state:?}")
                        }
                        EngineEvent::Error { pc, message } => {
                            println!("[{name}] ENGINE ERROR {pc:?}: {message}")
                        }
                        _ => {}
                    }
                }
            });
        }

        // Server events: subscribe offers (latest wins), ICE, roster.
        {
            let cmd = cmd_tx.clone();
            let engine = engine.clone();
            let streams = me.streams.clone();
            let answered = me.answered.clone();
            let name = handle.to_string();
            tokio::spawn(async move {
                while let Some(f) = event_rx.recv().await {
                    let data = &f["data"];
                    match f["type"].as_str().unwrap_or_default() {
                        "call.subscribe.offer" => {
                            let version = data["version"].as_u64().unwrap();
                            let mut map = HashMap::new();
                            for s in data["streams"].as_array().into_iter().flatten() {
                                map.insert(
                                    s["mid"].as_str().unwrap().to_string(),
                                    (
                                        s["participant_id"].as_str().unwrap().to_string(),
                                        s["kind"].as_str().unwrap().to_string(),
                                    ),
                                );
                            }
                            println!("[{name}] subscribe offer v{version}: {map:?}");
                            *streams.lock().unwrap() = map;
                            let sdp = data["sdp"].as_str().unwrap();
                            match engine.apply_subscribe_offer(sdp).await {
                                Ok(answer) => {
                                    let (tx, rx) = oneshot::channel();
                                    let _ = cmd.send((
                                        json!({"type": "call.subscribe.answer", "data": {
                                            "call_id": data["call_id"], "version": version,
                                            "sdp": answer}}),
                                        Some(tx),
                                    ));
                                    let reply = rx.await.unwrap_or_default();
                                    println!("[{name}] answer v{version} -> {}", reply["type"]);
                                    answered.store(version as usize, Ordering::SeqCst);
                                }
                                Err(err) => {
                                    println!("[{name}] APPLY OFFER v{version} FAILED: {err}")
                                }
                            }
                        }
                        "call.ice" => {
                            let pc = if data["pc"] == "publish" {
                                PcKind::Publish
                            } else {
                                PcKind::Subscribe
                            };
                            let c = data["candidate"].as_object().map(|c| IceCandidate {
                                candidate: c["candidate"].as_str().unwrap_or_default().into(),
                                sdp_mid: c.get("sdpMid").and_then(|v| v.as_str()).map(Into::into),
                                sdp_mline_index: c
                                    .get("sdpMLineIndex")
                                    .and_then(|v| v.as_u64())
                                    .map(|v| v as u32),
                            });
                            if let Err(e) = engine.add_remote_candidate(pc, c.as_ref()) {
                                println!("[{name}] remote ICE ({pc:?}) not applied: {e}");
                            }
                        }
                        "call.participant" => {
                            println!(
                                "[{name}] participant {} {}",
                                data["event"], data["participant"]["display_name"]
                            );
                        }
                        other => println!("[{name}] event {other}"),
                    }
                }
            });
        }

        // Publish.
        let offer = engine.create_publish_offer().await.unwrap();
        let answer = request(
            &cmd_tx,
            json!({"type": "call.publish", "data": {"call_id": call_id, "sdp": offer}}),
        )
        .await;
        assert_eq!(answer["type"], "call.publish.answer", "{handle}: {answer}");
        engine
            .apply_publish_answer(answer["data"]["sdp"].as_str().unwrap())
            .await
            .unwrap();
        println!("[{handle}] publishing");
        me
    }

    /// Wait until some remote video mid belonging to a stream in the latest
    /// offer has decoded at least `frames` more frames than it had at call time.
    async fn wait_video(&self, frames: usize) {
        let deadline = tokio::time::Instant::now() + TIMEOUT;
        let baseline = self.snapshot();
        loop {
            let now = self.snapshot();
            let active: Vec<String> = self
                .streams
                .lock()
                .unwrap()
                .iter()
                .filter(|(_, (_, k))| k == "video")
                .map(|(m, _)| m.clone())
                .collect();
            let ok = active.iter().any(|mid| {
                now.get(mid).copied().unwrap_or(0)
                    >= baseline.get(mid).copied().unwrap_or(0) + frames
            });
            if ok {
                println!("[{}] decoding remote video ({now:?})", self.name);
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "[{}] no remote video: counters {now:?}, active video mids {active:?}",
                self.name
            );
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    fn snapshot(&self) -> HashMap<String, usize> {
        self.counters
            .video
            .lock()
            .unwrap()
            .iter()
            .map(|(m, n)| (m.clone(), n.load(Ordering::Relaxed)))
            .collect()
    }

    async fn leave(self) {
        let (tx, rx) = oneshot::channel();
        let _ = self.cmd.send((
            json!({"type": "call.leave", "data": {"call_id": self.call_id}}),
            Some(tx),
        ));
        let reply = tokio::time::timeout(TIMEOUT, rx)
            .await
            .expect("leave reply")
            .unwrap();
        println!("[{}] leave -> {}", self.name, reply["type"]);
        self.engine.close();
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a running Brook server + Janus (see module docs)"]
async fn two_participants_and_renegotiation() {
    let env = env();
    let (a, b) = (&env.users[0], &env.users[1]);

    let alice = Participant::join(&env, &a.0, &a.1).await;
    let bob = Participant::join(&env, &b.0, &b.1).await;
    alice.wait_video(30).await;
    bob.wait_video(30).await;
    // Janus reused alice's mids for the new bob: the first bob's decode
    // chains must have been retired (torn down asynchronously).
    tokio::time::sleep(Duration::from_millis(500)).await;
    let mids = alice.streams.lock().unwrap().len();
    assert_eq!(
        alice.engine.subscribe_decoder_count(),
        mids,
        "stale decoders left behind"
    );

    // Bob leaves: Alice gets a re-offer without his streams.
    let answered = alice.answered.load(Ordering::SeqCst);
    bob.leave().await;
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    while alice.answered.load(Ordering::SeqCst) == answered {
        assert!(
            tokio::time::Instant::now() < deadline,
            "no re-offer after leave"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        alice.streams.lock().unwrap().is_empty(),
        "streams after leave: {:?}",
        alice.streams.lock().unwrap()
    );

    // Bob comes back: a re-offer adds (or reactivates) m-lines on Alice's
    // existing subscribe PC, and his video must decode again.
    let bob = Participant::join(&env, &b.0, &b.1).await;
    alice.wait_video(30).await;
    bob.wait_video(30).await;
    // Janus reused alice's mids for the new bob: the first bob's decode
    // chains must have been retired (torn down asynchronously).
    tokio::time::sleep(Duration::from_millis(500)).await;
    let mids = alice.streams.lock().unwrap().len();
    assert_eq!(
        alice.engine.subscribe_decoder_count(),
        mids,
        "stale decoders left behind"
    );

    bob.leave().await;
    alice.leave().await;
}
