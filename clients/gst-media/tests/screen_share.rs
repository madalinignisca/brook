//! Screen share on the publish PC: a share started mid-call adds a sendonly
//! video m-line by renegotiation (labelled `screen`), stopping it makes that
//! m-line inactive, and starting again reuses it. A second engine stands in
//! for the SFU (it answers the publish offers as a subscriber would).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use brook_core::{MediaKind, MediaSource};
use brook_media_gst::{
    CameraSource, EngineConfig, EngineEvent, GstEngine, MicSource, PcKind, ScreenSource,
    SinkFactory, TrackKind, VideoCodec,
};
use gst::prelude::*;

type Frames = Arc<Mutex<Vec<(String, Arc<AtomicUsize>)>>>;

fn sink(frames: Frames) -> SinkFactory {
    Arc::new(move |_kind| {
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
        frames.lock().unwrap().push((sink.name().to_string(), n));
        sink
    })
}

fn config(sink: SinkFactory) -> EngineConfig {
    EngineConfig {
        camera: CameraSource::Test,
        mic: MicSource::Test,
        codec: VideoCodec::H264,
        hardware_encode: false,
        video_kbps: 1000,
        ice_servers: vec![],
        video_sink: sink.clone(),
        audio_sink: Some(sink),
    }
}

fn video_mlines(sdp: &str) -> Vec<&str> {
    sdp.split("m=")
        .skip(1)
        .filter(|m| m.starts_with("video"))
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn share_start_stop_restart() {
    let pub_frames: Frames = Arc::default();
    let sub_frames: Frames = Arc::default();
    let (publisher, mut pub_events) = GstEngine::new(config(sink(pub_frames))).unwrap();
    let (subscriber, mut sub_events) = GstEngine::new(config(sink(sub_frames.clone()))).unwrap();

    // Remote video tracks the "SFU" side sees: mid -> sink name.
    let remote: Arc<Mutex<Vec<(String, TrackKind, String)>>> = Arc::default();
    {
        let sub = subscriber.clone();
        tokio::spawn(async move {
            while let Some(ev) = pub_events.recv().await {
                if let EngineEvent::LocalCandidate { candidate, .. } = ev {
                    let _ = sub.add_remote_candidate(PcKind::Subscribe, candidate.as_ref());
                }
            }
        });
        let publ = publisher.clone();
        let remote = remote.clone();
        tokio::spawn(async move {
            while let Some(ev) = sub_events.recv().await {
                match ev {
                    EngineEvent::LocalCandidate { candidate, .. } => {
                        let _ = publ.add_remote_candidate(PcKind::Publish, candidate.as_ref());
                    }
                    EngineEvent::RemoteTrack { mid, kind, sink } => {
                        remote
                            .lock()
                            .unwrap()
                            .push((mid, kind, sink.name().to_string()))
                    }
                    EngineEvent::Error { message, .. } => panic!("subscriber: {message}"),
                    _ => {}
                }
            }
        });
    }
    let negotiate = || async {
        let offer = publisher.create_publish_offer().await.unwrap();
        let answer = subscriber
            .apply_subscribe_offer(&offer, vec![])
            .await
            .unwrap();
        publisher.apply_publish_answer(&answer).await.unwrap();
        offer
    };
    let frames_of = |mid: &str| -> usize {
        let remote = remote.lock().unwrap();
        let frames = sub_frames.lock().unwrap();
        remote
            .iter()
            .rev()
            .find(|(m, k, _)| m == mid && *k == TrackKind::Video)
            .and_then(|(_, _, sink)| frames.iter().find(|(n, _)| n == sink))
            .map(|(_, n)| n.load(Ordering::Relaxed))
            .unwrap_or(0)
    };
    let wait_frames = |mid: String, more: usize| {
        let frames_of = &frames_of;
        async move {
            let base = frames_of(&mid);
            let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
            while frames_of(&mid) < base + more {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "no frames on mid {mid}"
                );
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    };

    // Camera call first: one video m-line, labelled camera.
    let offer = negotiate().await;
    assert_eq!(video_mlines(&offer).len(), 1);
    let tracks = publisher.publish_tracks();
    assert!(
        tracks.iter().all(|t| t.source != MediaSource::Screen),
        "{tracks:?}"
    );

    // Share: renegotiation adds a second, sendonly video m-line labelled screen.
    publisher.start_screen_share(ScreenSource::Test).unwrap();
    assert!(
        publisher.start_screen_share(ScreenSource::Test).is_err(),
        "double start"
    );
    // Labels are read right after the offer (core sends them with it),
    // before any answer: every one must already carry a real kind.
    let offer = publisher.create_publish_offer().await.unwrap();
    let early = publisher.publish_tracks();
    assert!(
        early.iter().all(|t| t.kind != MediaKind::Unknown),
        "labels right after the offer: {early:?}"
    );
    let answer = subscriber
        .apply_subscribe_offer(&offer, vec![])
        .await
        .unwrap();
    publisher.apply_publish_answer(&answer).await.unwrap();
    let video = video_mlines(&offer);
    assert_eq!(video.len(), 2, "{offer}");
    assert!(video[1].contains("a=sendonly"), "{offer}");
    let tracks = publisher.publish_tracks();
    let screen: Vec<_> = tracks
        .iter()
        .filter(|t| t.source == MediaSource::Screen)
        .collect();
    assert_eq!(screen.len(), 1, "{tracks:?}");
    assert_eq!(screen[0].kind, MediaKind::Video);
    // Every label must carry a real kind: the server rejects anything else.
    assert!(
        tracks.iter().all(|t| t.kind != MediaKind::Unknown),
        "{tracks:?}"
    );
    let screen_mid = screen[0].mid.clone();
    wait_frames(screen_mid.clone(), 10).await;

    // Stop: that m-line goes inactive; it stays labelled screen (the wire
    // requires every m-line labelled; the server ignores inactive ones).
    publisher.stop_screen_share().unwrap();
    let offer = negotiate().await;
    assert!(video_mlines(&offer)[1].contains("a=inactive"), "{offer}");
    let tracks = publisher.publish_tracks();
    assert_eq!(tracks.len(), 3, "every m-line labelled: {tracks:?}");
    assert!(tracks
        .iter()
        .any(|t| t.mid == screen_mid && t.source == MediaSource::Screen));

    // Share again: the same m-line comes back (no m-line growth).
    publisher.start_screen_share(ScreenSource::Test).unwrap();
    let offer = negotiate().await;
    assert_eq!(video_mlines(&offer).len(), 2, "{offer}");
    let tracks = publisher.publish_tracks();
    let again = tracks
        .iter()
        .find(|t| t.source == MediaSource::Screen)
        .expect("screen labelled again");
    assert_eq!(again.mid, screen_mid, "the screen m-line is reused");

    publisher.close();
    subscriber.close();
}

/// A screen capture that fails (the user ended it from the desktop's sharing
/// indicator) ends the share only: ScreenShareEnded, never a call-level Error.
#[tokio::test(flavor = "multi_thread")]
async fn a_failing_capture_ends_only_the_share() {
    let frames: Frames = Arc::default();
    let (engine, mut events) = GstEngine::new(config(sink(frames))).unwrap();
    // One offer builds the publish pipeline; no answer is needed for this
    // check (and offering again without one would leave webrtcbin stuck in
    // have-local-offer, which core never does).
    engine.create_publish_offer().await.unwrap();
    engine.start_screen_share(ScreenSource::Test).unwrap();
    engine.fail_screen_capture_for_test();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let ev = tokio::time::timeout_at(deadline, events.recv())
            .await
            .expect("ScreenShareEnded within 5 s")
            .unwrap();
        match ev {
            EngineEvent::ScreenShareEnded { .. } => break,
            EngineEvent::Error { message, .. } => panic!("call-level error: {message}"),
            _ => {}
        }
    }
    // The share can be stopped cleanly and the engine keeps working.
    engine.stop_screen_share().unwrap();
    assert!(engine.set_local_media(true, true).is_ok());
    engine.close();
}
