//! The engine's obligations under core's `MediaEngine` contract.

use std::sync::Arc;

use brook_core::MediaEngine;
use brook_media_gst::{CameraSource, EngineConfig, GstEngine, MicSource, VideoCodec};

fn engine() -> Arc<GstEngine> {
    let sink = Arc::new(|_| gst::ElementFactory::make("fakesink").build().unwrap());
    GstEngine::new(EngineConfig {
        camera: CameraSource::Test,
        mic: MicSource::Test,
        codec: VideoCodec::Vp8,
        hardware_encode: false,
        video_kbps: 500,
        ice_servers: vec![],
        video_sink: sink.clone(),
        audio_sink: Some(sink),
    })
    .unwrap()
    .0
}

/// Core holds it as a trait object.
#[tokio::test(flavor = "multi_thread")]
async fn usable_as_dyn_media_engine() {
    let engine: Arc<dyn MediaEngine> = engine();
    let offer = engine.create_publish_offer().await.unwrap();
    assert!(offer.contains("a=sendonly"));
    engine.close().await;
}

/// After close, nothing succeeds and nothing is rebuilt.
#[tokio::test(flavor = "multi_thread")]
async fn close_fences_later_operations() {
    let engine = engine();
    engine.close();
    assert!(engine.create_publish_offer().await.is_err());
    assert!(engine
        .apply_subscribe_offer("v=0\r\n", vec![])
        .await
        .is_err());
    assert_eq!(engine.subscribe_decoder_count(), 0);
}

/// An operation in flight when close() lands must fail, not hand back an SDP
/// for a PC that no longer exists.
#[tokio::test(flavor = "multi_thread")]
async fn close_fences_an_in_flight_offer() {
    let engine = engine();
    let pending = {
        let engine = engine.clone();
        tokio::spawn(async move { engine.create_publish_offer().await })
    };
    // The offer waits for capture caps first; close while it does.
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    engine.close();
    assert!(
        pending.await.unwrap().is_err(),
        "offer succeeded after close"
    );
}

/// The machine's real camera through `CameraSource::Auto` produces frames.
/// Ignored: needs a camera (run on a dev machine).
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a real camera"]
async fn auto_camera_produces_frames() {
    use gst::prelude::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let frames = Arc::new(AtomicUsize::new(0));
    let counter = frames.clone();
    let sink = Arc::new(move |_| {
        let sink = gst::ElementFactory::make("fakesink")
            .property("signal-handoffs", true)
            .build()
            .unwrap();
        let c = counter.clone();
        sink.connect("handoff", false, move |_| {
            c.fetch_add(1, Ordering::Relaxed);
            None
        });
        sink
    });
    let (engine, mut events) = GstEngine::new(EngineConfig {
        camera: CameraSource::Auto,
        mic: MicSource::None,
        codec: VideoCodec::H264,
        hardware_encode: true,
        video_kbps: 1500,
        ice_servers: vec![],
        video_sink: sink.clone(),
        audio_sink: Some(sink),
    })
    .unwrap();
    let offer = engine
        .create_publish_offer()
        .await
        .expect("offer with a real camera");
    assert!(offer.contains("H264/90000"));
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    while let Ok(ev) = events.try_recv() {
        if let brook_media_gst::EngineEvent::Error { message, .. } = ev {
            panic!("camera pipeline error: {message}");
        }
    }
    assert!(
        frames.load(Ordering::Relaxed) > 20,
        "self-view got too few frames"
    );

    // Camera off releases the device (this process no longer holds any
    // /dev/video*); camera on brings frames back.
    let holds_camera = || {
        let me = std::process::id().to_string();
        let out = std::process::Command::new("fuser")
            .args(["/dev/video0", "/dev/video1", "/dev/video2", "/dev/video3"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
            .unwrap_or_default();
        out.split_whitespace()
            .any(|p| p.trim_end_matches(|c: char| !c.is_ascii_digit()) == me)
    };
    assert!(holds_camera(), "fuser check can't see the open camera");
    engine.set_local_media(false, false).unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    assert!(!holds_camera(), "camera device still open while off");
    let before = frames.load(Ordering::Relaxed);
    engine.set_local_media(false, true).unwrap();
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    assert!(
        frames.load(Ordering::Relaxed) > before + 20,
        "no frames after camera back on"
    );
    engine.close();
}

fn engine_with(camera: CameraSource, mic: MicSource) -> Arc<GstEngine> {
    let sink = Arc::new(|_| gst::ElementFactory::make("fakesink").build().unwrap());
    GstEngine::new(EngineConfig {
        camera,
        mic,
        codec: VideoCodec::Vp8,
        hardware_encode: false,
        video_kbps: 500,
        ice_servers: vec![],
        video_sink: sink.clone(),
        audio_sink: Some(sink),
    })
    .unwrap()
    .0
}

/// Privacy: camera off must release the capture device (its LED goes off),
/// not just stop sending frames; camera on captures again.
#[tokio::test(flavor = "multi_thread")]
async fn camera_off_releases_the_source() {
    let engine = engine_with(CameraSource::Test, MicSource::Test);
    engine.create_publish_offer().await.unwrap();
    assert!(engine.camera_capturing(), "camera should capture while on");
    engine.set_local_media(true, false).unwrap();
    assert!(
        !engine.camera_capturing(),
        "camera still capturing while off"
    );
    engine.set_local_media(true, true).unwrap();
    assert!(engine.camera_capturing(), "camera did not restart");
    engine.close();
}

/// Enabling a track that isn't published is an error, so core doesn't
/// announce media that isn't there. Disabling it is fine.
#[tokio::test(flavor = "multi_thread")]
async fn enabling_an_unpublished_track_fails() {
    let engine = engine_with(CameraSource::None, MicSource::Test);
    engine.create_publish_offer().await.unwrap();
    assert!(engine.set_local_media(true, false).is_ok());
    assert!(engine.set_local_media(false, false).is_ok());
    assert!(
        engine.set_local_media(true, true).is_err(),
        "video on without a camera"
    );

    let engine = engine_with(CameraSource::Test, MicSource::None);
    engine.create_publish_offer().await.unwrap();
    assert!(engine.set_local_media(false, true).is_ok());
    assert!(
        engine.set_local_media(true, true).is_err(),
        "audio on without a mic"
    );
}
