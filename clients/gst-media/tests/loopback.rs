//! In-process loopback: one engine publishes synthetic mic + camera, a second
//! engine subscribes, with SDP and ICE shuttled the way `core` will shuttle
//! them over the WS (PROTOCOL.md §3). Proves capture → encode → DTLS/SRTP →
//! decode → sink on this machine, with no server and no display.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use brook_media_gst::{
    CameraSource, EngineConfig, EngineEvent, GstEngine, MicSource, PcKind, SinkFactory, TrackKind,
    VideoCodec,
};
use gst::prelude::*;

/// A `fakesink` factory that counts buffers per media kind.
fn counting_sink(video: Arc<AtomicUsize>, audio: Arc<AtomicUsize>) -> SinkFactory {
    Arc::new(move |kind| {
        let sink = gst::ElementFactory::make("fakesink")
            .property("sync", false)
            .property("signal-handoffs", true)
            .build()
            .unwrap();
        let counter = match kind {
            TrackKind::Video => video.clone(),
            TrackKind::Audio => audio.clone(),
        };
        sink.connect("handoff", false, move |_| {
            counter.fetch_add(1, Ordering::Relaxed);
            None
        });
        sink
    })
}

fn config(codec: VideoCodec, sink: SinkFactory) -> EngineConfig {
    EngineConfig {
        camera: CameraSource::Test,
        mic: MicSource::Test,
        codec,
        hardware_encode: false,
        video_kbps: 1500,
        ice_servers: vec![],
        video_sink: sink.clone(),
        audio_sink: Some(sink),
    }
}

async fn loopback(codec: VideoCodec) {
    let (pub_v, pub_a) = (Arc::default(), Arc::default());
    let (sub_v, sub_a): (Arc<AtomicUsize>, Arc<AtomicUsize>) = (Arc::default(), Arc::default());
    let (publisher, mut pub_events) =
        GstEngine::new(config(codec, counting_sink(pub_v, pub_a))).unwrap();
    let (subscriber, mut sub_events) =
        GstEngine::new(config(codec, counting_sink(sub_v.clone(), sub_a.clone()))).unwrap();

    // Signaling, as core does it: publisher offers, the "SFU" (here: the
    // subscriber engine) answers.
    let offer = publisher.create_publish_offer().await.unwrap();
    let expected = match codec {
        VideoCodec::H264 => "profile-level-id=42e01f",
        VideoCodec::Vp8 => "VP8/90000",
    };
    assert!(
        offer.contains("a=sendonly"),
        "publish offer must be sendonly:\n{offer}"
    );
    assert!(offer.contains(expected), "offer lacks {expected}:\n{offer}");
    assert!(
        offer.to_lowercase().contains("opus/48000"),
        "offer lacks Opus:\n{offer}"
    );
    let answer = subscriber
        .apply_subscribe_offer(&offer, vec![])
        .await
        .unwrap();
    publisher.apply_publish_answer(&answer).await.unwrap();

    // Trickle ICE both ways; collect remote tracks.
    let sub = subscriber.clone();
    tokio::spawn(async move {
        while let Some(ev) = pub_events.recv().await {
            if let EngineEvent::LocalCandidate {
                pc: PcKind::Publish,
                candidate,
            } = ev
            {
                sub.add_remote_candidate(PcKind::Subscribe, candidate.as_ref())
                    .unwrap();
            }
        }
    });
    let publ = publisher.clone();
    let (tracks_tx, mut tracks_rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(ev) = sub_events.recv().await {
            match ev {
                EngineEvent::LocalCandidate {
                    pc: PcKind::Subscribe,
                    candidate,
                } => {
                    publ.add_remote_candidate(PcKind::Publish, candidate.as_ref())
                        .unwrap();
                }
                EngineEvent::RemoteTrack { mid, kind, .. } => {
                    let _ = tracks_tx.send((mid, kind));
                }
                EngineEvent::Error { message, .. } => panic!("subscriber error: {message}"),
                _ => {}
            }
        }
    });

    let mut kinds = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while kinds.len() < 2 {
        let (mid, kind) = tokio::time::timeout_at(deadline, tracks_rx.recv())
            .await
            .expect("remote tracks within 20s")
            .unwrap();
        assert!(!mid.is_empty(), "remote track without a mid");
        kinds.push(kind);
    }
    assert!(kinds.contains(&TrackKind::Audio) && kinds.contains(&TrackKind::Video));

    // Decoded frames flow.
    while sub_v.load(Ordering::Relaxed) < 30 || sub_a.load(Ordering::Relaxed) < 10 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "decoded media within 20s"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // Camera off stops video; on resumes it.
    publisher.set_local_media(true, false).unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    let paused = sub_v.load(Ordering::Relaxed);
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert!(
        sub_v.load(Ordering::Relaxed) - paused <= 3,
        "video kept flowing while off"
    );
    publisher.set_local_media(true, true).unwrap();
    let resumed = sub_v.load(Ordering::Relaxed);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while sub_v.load(Ordering::Relaxed) < resumed + 10 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "video resumes after camera on"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    publisher.close();
    subscriber.close();
}

#[tokio::test(flavor = "multi_thread")]
async fn loopback_h264_opus() {
    loopback(VideoCodec::H264).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn loopback_vp8_opus() {
    loopback(VideoCodec::Vp8).await;
}
