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
