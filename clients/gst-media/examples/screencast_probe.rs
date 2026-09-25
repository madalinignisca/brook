//! Manual check of a real screen share through the engine: asks the desktop
//! for a screen or window (portal picker + consent), shares it on a publish
//! PC, and a second engine (standing in for the SFU) decodes it. Prints the
//! decoded frame count, then stops the share (the desktop's sharing indicator
//! should disappear). `cargo run -p brook-media-gst --example screencast_probe`

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use brook_media_gst::{
    request_screen_cast, CameraSource, EngineConfig, EngineEvent, GstEngine, MediaSource,
    MicSource, PcKind, SinkFactory, TrackKind, VideoCodec,
};
use gst::prelude::*;

/// Decoded frames per sink name.
type Counters = Arc<Mutex<Vec<(String, Arc<AtomicUsize>)>>>;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let counters: Counters = Arc::default();
    let sink: SinkFactory = {
        let counters = counters.clone();
        Arc::new(move |_| {
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
            counters.lock().unwrap().push((sink.name().to_string(), n));
            sink
        })
    };
    let config = |sink: SinkFactory| EngineConfig {
        camera: CameraSource::None,
        mic: MicSource::None,
        codec: VideoCodec::H264,
        hardware_encode: std::env::var("BROOK_HW_ENCODE").as_deref() == Ok("1"),
        video_kbps: 1500,
        ice_servers: vec![],
        video_sink: sink.clone(),
        audio_sink: Some(sink),
    };
    let quiet: SinkFactory = Arc::new(|_| gst::ElementFactory::make("fakesink").build().unwrap());
    // A test tone so the first offer has an m-line to start from.
    let mut pub_config = config(quiet);
    pub_config.mic = MicSource::Test;
    let (publisher, mut pub_events) = GstEngine::new(pub_config)?;
    let (subscriber, mut sub_events) = GstEngine::new(config(sink))?;

    let remote: Arc<Mutex<Vec<(String, String)>>> = Arc::default();
    {
        let sub = subscriber.clone();
        tokio::spawn(async move {
            while let Some(ev) = pub_events.recv().await {
                match ev {
                    EngineEvent::LocalCandidate { candidate, .. } => {
                        let _ = sub.add_remote_candidate(PcKind::Subscribe, candidate.as_ref());
                    }
                    EngineEvent::Error { message, .. } => eprintln!("publisher error: {message}"),
                    _ => {}
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
                    EngineEvent::RemoteTrack {
                        mid,
                        kind: TrackKind::Video,
                        sink,
                    } => remote.lock().unwrap().push((mid, sink.name().to_string())),
                    EngineEvent::Error { message, .. } => eprintln!("subscriber error: {message}"),
                    _ => {}
                }
            }
        });
    }

    // A publish PC with nothing on it yet, then the share.
    let offer = publisher.create_publish_offer().await?;
    let answer = subscriber.apply_subscribe_offer(&offer, vec![]).await?;
    publisher.apply_publish_answer(&answer).await?;
    println!("pick a screen or window in the dialog...");
    let source = request_screen_cast().await?;
    publisher.start_screen_share(source)?;
    let offer = publisher.create_publish_offer().await?;
    let answer = subscriber.apply_subscribe_offer(&offer, vec![]).await?;
    publisher.apply_publish_answer(&answer).await?;
    let tracks = publisher.publish_tracks();
    println!("publish tracks: {tracks:?}");
    let screen_mid = tracks
        .iter()
        .find(|t| t.source == MediaSource::Screen)
        .map(|t| t.mid.clone())
        .ok_or("no screen track")?;

    tokio::time::sleep(Duration::from_secs(6)).await;
    let frames = {
        let remote = remote.lock().unwrap();
        let counters = counters.lock().unwrap();
        remote
            .iter()
            .rev()
            .find(|(mid, _)| *mid == screen_mid)
            .and_then(|(_, sink)| counters.iter().find(|(n, _)| n == sink))
            .map(|(_, n)| n.load(Ordering::Relaxed))
            .unwrap_or(0)
    };
    println!("decoded screen frames in ~6 s at 10 fps: {frames}");

    if let Some(session) = publisher.stop_screen_share()? {
        session.close().await;
    }
    println!("share stopped");
    publisher.close();
    subscriber.close();
    Ok(())
}
