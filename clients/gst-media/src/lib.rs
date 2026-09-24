//! Brook call media engine for Linux clients, on GStreamer `webrtcbin`.
//!
//! Implements the per-client half of [MEDIA.md](../../../docs/MEDIA.md): capture,
//! encode, SRTP/DTLS transport, decode and render. It speaks SDP and ICE only;
//! call signaling (who offers when, versions, the WS) belongs to `core`, which
//! drives this engine through its `MediaEngine` trait (PROTOCOL.md §3).
//!
//! Two PeerConnections, as the contract requires, each its own pipeline:
//! - **publish**: `sendonly` mic + camera. We create the offer, the SFU answers.
//! - **subscribe**: `recvonly`, all remote streams. The SFU offers, we answer,
//!   and it re-offers on the same PC whenever the roster changes.
//!
//! The engine never touches a toolkit: video sinks come from the app through
//! [`SinkFactory`] (GTK: `gtk4paintablesink`, tests: `fakesink`), and everything
//! it has to say (local ICE, new remote tracks, errors) is sent as an
//! [`EngineEvent`] on a channel. Every method is safe to call from any thread.
//!
//! [`GstEngine`] implements core's [`brook_core::MediaEngine`], so a client
//! hands it to `BrookClient::join_call` and core drives it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gst::prelude::*;
use tokio::sync::{mpsc, oneshot};

pub use brook_core::{IceCandidate, IceServer, PcKind, SubStream};

/// Result alias for engine operations.
pub type Result<T> = std::result::Result<T, Error>;

/// Engine failures. The message is for logs; `core` maps any error to a
/// call-level outcome.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// GStreamer could not be initialized or a required element is missing.
    #[error("media setup failed: {0}")]
    Setup(String),
    /// SDP could not be parsed or was rejected by webrtcbin.
    #[error("sdp: {0}")]
    Sdp(String),
    /// A method was called in the wrong order (e.g. an answer before an offer),
    /// or after [`GstEngine::close`].
    #[error("bad state: {0}")]
    State(&'static str),
}

impl From<Error> for brook_core::EngineError {
    fn from(err: Error) -> Self {
        brook_core::EngineError(err.to_string())
    }
}

/// Media kind of a track.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TrackKind {
    /// Opus audio.
    Audio,
    /// H.264 / VP8 video.
    Video,
}

/// Where the outgoing video comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CameraSource {
    /// Let GStreamer pick the default camera (`autovideosrc`).
    Auto,
    /// A specific V4L2 device, e.g. `/dev/video0`.
    Device(String),
    /// A synthetic pattern (tests, machines without a camera).
    Test,
    /// Don't publish video at all.
    None,
}

/// Where the outgoing audio comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MicSource {
    /// The default input (`autoaudiosrc`; PipeWire on modern desktops).
    Auto,
    /// A synthetic tone (tests).
    Test,
    /// Don't publish audio at all.
    None,
}

/// Outgoing video codec. H.264 is the contract's primary; VP8 the fallback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoCodec {
    /// H.264 constrained baseline, advertised as `profile-level-id=42e01f`.
    H264,
    /// VP8 (software), for machines without a usable H.264 encoder.
    Vp8,
}

/// Builds the element a decoded (or local preview) video stream renders into.
/// Called on GStreamer threads; the element must be safe to create there
/// (`gtk4paintablesink` is).
pub type SinkFactory = Arc<dyn Fn(TrackKind) -> gst::Element + Send + Sync>;

/// Engine configuration.
#[derive(Clone)]
pub struct EngineConfig {
    /// Outgoing camera.
    pub camera: CameraSource,
    /// Outgoing microphone.
    pub mic: MicSource,
    /// Outgoing video codec.
    pub codec: VideoCodec,
    /// Try VA-API hardware H.264 first (opt-in until validated per machine).
    pub hardware_encode: bool,
    /// Outgoing video bitrate cap, kbit/s (the SFU room caps at 1500).
    pub video_kbps: u32,
    /// STUN/TURN servers; empty = host candidates only (fine on a LAN).
    pub ice_servers: Vec<IceServer>,
    /// Video sink factory (remote tiles and the local self-view).
    pub video_sink: SinkFactory,
    /// Audio sink factory for remote audio. `None` = `autoaudiosink`.
    pub audio_sink: Option<SinkFactory>,
}

impl std::fmt::Debug for EngineConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EngineConfig")
            .field("camera", &self.camera)
            .field("mic", &self.mic)
            .field("codec", &self.codec)
            .field("hardware_encode", &self.hardware_encode)
            .field("video_kbps", &self.video_kbps)
            .field("ice_servers", &self.ice_servers.len())
            .finish_non_exhaustive()
    }
}

/// What the engine reports back. `gst::Element`s are handed out (they are
/// `Send`) so the UI thread can read e.g. a sink's `paintable` itself.
#[derive(Debug, Clone)]
pub enum EngineEvent {
    /// A local ICE candidate to send as `call.ice`; `None` = end-of-candidates.
    LocalCandidate {
        /// Which PC it belongs to.
        pc: PcKind,
        /// The candidate, or `None` when gathering completed.
        candidate: Option<IceCandidate>,
    },
    /// The self-view sink is ready (created by the [`SinkFactory`]).
    LocalPreview {
        /// The sink element (e.g. `gtk4paintablesink`).
        sink: gst::Element,
    },
    /// A remote track started decoding. Map `mid` to a participant through the
    /// latest `call.subscribe.offer` streams. Audio plays by itself; `sink` is
    /// the element it was linked into.
    RemoteTrack {
        /// The transceiver mid in our subscribe PC.
        mid: String,
        /// Audio or video.
        kind: TrackKind,
        /// The sink element the decoded stream feeds.
        sink: gst::Element,
    },
    /// The subscribe PC applied an offer whose mids map to these streams (a
    /// mid absent here is inactive: drop its tile). Sent before the answer is
    /// returned, so it precedes that offer's [`EngineEvent::RemoteTrack`]s.
    SubscribeStreams(Vec<SubStream>),
    /// A PC's connection state changed (`new`, `connecting`, `connected`,
    /// `disconnected`, `failed`, `closed`).
    ConnectionState {
        /// Which PC.
        pc: PcKind,
        /// The new state.
        state: gst_webrtc::WebRTCPeerConnectionState,
    },
    /// A pipeline error (device gone, encoder failure, ...).
    Error {
        /// Which PC's pipeline failed.
        pc: PcKind,
        /// Human-readable detail for logs.
        message: String,
    },
}

/// The engine: owns both PCs and the event channel.
pub struct GstEngine {
    config: EngineConfig,
    events: mpsc::UnboundedSender<EngineEvent>,
    publish: Mutex<Option<Pc>>,
    subscribe: Mutex<Option<Pc>>,
    /// STUN/TURN for PCs built from now on ([`GstEngine::set_ice_servers`]).
    ice_servers: Mutex<Vec<IceServer>>,
    /// Set by [`GstEngine::close`]: nothing builds a PC or succeeds afterwards.
    closed: AtomicBool,
}

/// One PeerConnection: a pipeline with a `webrtcbin` named `webrtc`.
struct Pc {
    pipeline: gst::Pipeline,
    webrtc: gst::Element,
}

impl Drop for Pc {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

/// How long to wait for capture/encoder caps before offering anyway.
const CAPS_TIMEOUT: Duration = Duration::from_secs(10);

impl GstEngine {
    /// Create an engine. Initializes GStreamer and checks for `webrtcbin`.
    pub fn new(config: EngineConfig) -> Result<(Arc<Self>, mpsc::UnboundedReceiver<EngineEvent>)> {
        gst::init().map_err(|e| Error::Setup(e.to_string()))?;
        if gst::ElementFactory::find("webrtcbin").is_none() {
            return Err(Error::Setup(
                "GStreamer webrtcbin is missing (install gst-plugins-bad + libnice)".into(),
            ));
        }
        let (tx, rx) = mpsc::unbounded_channel();
        Ok((
            Arc::new(Self {
                ice_servers: Mutex::new(config.ice_servers.clone()),
                config,
                events: tx,
                publish: Mutex::new(None),
                subscribe: Mutex::new(None),
                closed: AtomicBool::new(false),
            }),
            rx,
        ))
    }

    /// Create (or, on an existing publish PC, re-create for renegotiation) the
    /// publish offer. Builds the capture pipeline on first use.
    pub async fn create_publish_offer(&self) -> Result<String> {
        let webrtc = {
            let mut guard = self.publish.lock().unwrap();
            self.ensure_open()?;
            if guard.is_none() {
                *guard = Some(self.build_publish()?);
            }
            guard.as_ref().unwrap().webrtc.clone()
        };
        wait_for_sink_caps(&webrtc, &self.closed).await;
        self.ensure_open()?;
        // The publish PC only sends (PROTOCOL.md §3.1).
        for t in transceivers(&webrtc) {
            t.set_property(
                "direction",
                gst_webrtc::WebRTCRTPTransceiverDirection::Sendonly,
            );
        }

        let offer = create_description(&webrtc, "create-offer", "offer").await?;
        set_description(&webrtc, "set-local-description", &offer).await?;
        self.ensure_open()?;
        sdp_text(&offer)
    }

    /// Apply the SFU's answer to our publish offer.
    pub async fn apply_publish_answer(&self, sdp: &str) -> Result<()> {
        let webrtc = self
            .webrtc(PcKind::Publish)
            .ok_or(Error::State("publish answer before offer"))?;
        let answer = parse_description(gst_webrtc::WebRTCSDPType::Answer, sdp)?;
        set_description(&webrtc, "set-remote-description", &answer).await?;
        self.ensure_open()
    }

    /// Apply an SFU offer on the subscribe PC (first or renegotiation) and
    /// return our answer. `streams` maps the offer's mids to participants and
    /// is re-emitted as [`EngineEvent::SubscribeStreams`] for the UI.
    pub async fn apply_subscribe_offer(
        &self,
        sdp: &str,
        streams: Vec<SubStream>,
    ) -> Result<String> {
        let webrtc = {
            let mut guard = self.subscribe.lock().unwrap();
            self.ensure_open()?;
            if guard.is_none() {
                *guard = Some(self.build_subscribe()?);
            }
            guard.as_ref().unwrap().webrtc.clone()
        };
        let offer = parse_description(gst_webrtc::WebRTCSDPType::Offer, sdp)?;
        set_description(&webrtc, "set-remote-description", &offer).await?;
        let answer = create_description(&webrtc, "create-answer", "answer").await?;
        set_description(&webrtc, "set-local-description", &answer).await?;
        self.ensure_open()?;
        let _ = self.events.send(EngineEvent::SubscribeStreams(streams));
        sdp_text(&answer)
    }

    /// Add a remote (SFU) candidate. `None` (end-of-candidates) is accepted
    /// and ignored: webrtcbin needs no signal for it and the SFU is ICE-lite.
    pub fn add_remote_candidate(&self, pc: PcKind, candidate: Option<&IceCandidate>) -> Result<()> {
        let Some(c) = candidate else { return Ok(()) };
        let webrtc = self
            .webrtc(pc)
            .ok_or(Error::State("candidate for a PC that doesn't exist"))?;
        let mline = match (c.sdp_mline_index, &c.sdp_mid) {
            (Some(i), _) => i,
            (None, Some(mid)) => mline_for_mid(&webrtc, mid).unwrap_or(0),
            // Bundled: every m-line shares the first transport.
            (None, None) => 0,
        };
        webrtc.emit_by_name::<()>("add-ice-candidate", &[&mline, &c.candidate]);
        Ok(())
    }

    /// Local mute / camera toggle as the user sees it. Muted audio sends
    /// silence; a disabled camera stops sending frames. Turning the camera
    /// back on asks the encoder for a keyframe so receivers recover at once.
    pub fn set_local_media(&self, audio: bool, video: bool) -> Result<()> {
        let guard = self.publish.lock().unwrap();
        let Some(pc) = guard.as_ref() else {
            return Ok(());
        };
        if let Some(vol) = pc.pipeline.by_name("mic_volume") {
            vol.set_property("mute", !audio);
        }
        if let Some(valve) = pc.pipeline.by_name("cam_valve") {
            let was_dropping = valve.property::<bool>("drop");
            valve.set_property("drop", !video);
            if video && was_dropping {
                request_keyframe(&pc.pipeline);
            }
        }
        Ok(())
    }

    /// Number of remote decode chains (decodebins) in the subscribe pipeline.
    /// For tests: retired chains must not accumulate across re-offers.
    #[doc(hidden)]
    pub fn subscribe_decoder_count(&self) -> usize {
        let guard = self.subscribe.lock().unwrap();
        let Some(pc) = guard.as_ref() else { return 0 };
        pc.pipeline
            .iterate_elements()
            .into_iter()
            .flatten()
            .filter(|e| e.factory().is_some_and(|f| f.name() == "decodebin"))
            .count()
    }

    /// STUN/TURN servers for PCs created from now on (an existing PC keeps
    /// its own; the SFU is reachable without them on a LAN).
    pub fn set_ice_servers(&self, servers: Vec<IceServer>) {
        *self.ice_servers.lock().unwrap() = servers;
    }

    /// Tear both PCs down (leave / call ended) and fence the engine: any
    /// operation still in flight, or started later, fails with
    /// [`Error::State`] and builds nothing.
    pub fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        self.publish.lock().unwrap().take();
        self.subscribe.lock().unwrap().take();
    }

    fn ensure_open(&self) -> Result<()> {
        if self.closed.load(Ordering::SeqCst) {
            Err(Error::State("engine closed"))
        } else {
            Ok(())
        }
    }

    fn webrtc(&self, pc: PcKind) -> Option<gst::Element> {
        let slot = match pc {
            PcKind::Publish => &self.publish,
            PcKind::Subscribe => &self.subscribe,
        };
        slot.lock().unwrap().as_ref().map(|pc| pc.webrtc.clone())
    }

    fn build_publish(&self) -> Result<Pc> {
        let desc = publish_description(&self.config)?;
        tracing::debug!(pipeline = %desc, "building publish pipeline");
        let pipeline = gst::parse::launch(&desc)
            .map_err(|e| Error::Setup(format!("publish pipeline: {e}")))?
            .downcast::<gst::Pipeline>()
            .map_err(|_| Error::Setup("publish pipeline is not a pipeline".into()))?;
        let webrtc = pipeline.by_name("webrtc").expect("webrtcbin named webrtc");

        // Self-view: tee branch ending in an app-provided sink.
        if let Some(queue) = pipeline.by_name("preview_q") {
            let sink = (self.config.video_sink)(TrackKind::Video);
            sink.set_property("sync", false);
            let convert = make("videoconvert")?;
            pipeline
                .add_many([&convert, &sink])
                .map_err(|e| Error::Setup(e.to_string()))?;
            gst::Element::link_many([&queue, &convert, &sink])
                .map_err(|e| Error::Setup(format!("link preview: {e}")))?;
            let _ = self.events.send(EngineEvent::LocalPreview { sink });
        }

        self.wire_pc(PcKind::Publish, &pipeline, &webrtc)?;
        pipeline
            .set_state(gst::State::Playing)
            .map_err(|e| Error::Setup(format!("start publish pipeline: {e}")))?;
        Ok(Pc { pipeline, webrtc })
    }

    fn build_subscribe(&self) -> Result<Pc> {
        let pipeline = gst::Pipeline::with_name("brook-subscribe");
        let webrtc = gst::ElementFactory::make("webrtcbin")
            .name("webrtc")
            .property_from_str("bundle-policy", "max-bundle")
            .build()
            .map_err(|e| Error::Setup(e.to_string()))?;
        pipeline
            .add(&webrtc)
            .map_err(|e| Error::Setup(e.to_string()))?;

        let events = self.events.clone();
        let video_sink = self.config.video_sink.clone();
        let audio_sink = self.config.audio_sink.clone();
        let chains = Chains::default();
        let pipeline_weak = pipeline.downgrade();
        let added_chains = chains.clone();
        webrtc.connect_pad_added(move |_webrtc, pad| {
            if pad.direction() != gst::PadDirection::Src {
                return;
            }
            let Some(pipeline) = pipeline_weak.upgrade() else {
                return;
            };
            let mid = pad
                .property::<Option<gst_webrtc::WebRTCRTPTransceiver>>("transceiver")
                .and_then(|t| t.property::<Option<String>>("mid"))
                .unwrap_or_default();
            // The SFU reuses mids across re-offers (a participant leaves, a
            // new one takes the m-line): retire the previous decode chain.
            let old = added_chains.lock().unwrap().remove(&mid);
            if let Some(old) = old {
                teardown(&pipeline, old);
            }
            if let Err(err) = link_remote(
                &pipeline,
                pad,
                mid,
                &added_chains,
                video_sink.clone(),
                audio_sink.clone(),
                events.clone(),
            ) {
                let _ = events.send(EngineEvent::Error {
                    pc: PcKind::Subscribe,
                    message: err.to_string(),
                });
            }
        });

        let pipeline_weak = pipeline.downgrade();
        webrtc.connect_pad_removed(move |_webrtc, pad| {
            let Some(pipeline) = pipeline_weak.upgrade() else {
                return;
            };
            let mut chains = chains.lock().unwrap();
            let mid = chains
                .iter()
                .find(|(_, c)| &c.pad == pad)
                .map(|(mid, _)| mid.clone());
            if let Some(chain) = mid.and_then(|mid| chains.remove(&mid)) {
                teardown(&pipeline, chain);
            }
        });

        self.wire_pc(PcKind::Subscribe, &pipeline, &webrtc)?;
        pipeline
            .set_state(gst::State::Playing)
            .map_err(|e| Error::Setup(format!("start subscribe pipeline: {e}")))?;
        Ok(Pc { pipeline, webrtc })
    }

    /// Common PC wiring: ICE servers, local candidates, state and bus errors.
    fn wire_pc(&self, kind: PcKind, pipeline: &gst::Pipeline, webrtc: &gst::Element) -> Result<()> {
        apply_ice_servers(webrtc, &self.ice_servers.lock().unwrap());

        let events = self.events.clone();
        webrtc.connect("on-ice-candidate", false, move |values| {
            let webrtc = values[0].get::<gst::Element>().ok()?;
            let mline = values[1].get::<u32>().ok()?;
            let candidate = values[2].get::<String>().ok()?;
            let sdp_mid = transceiver_mid(&webrtc, mline);
            let _ = events.send(EngineEvent::LocalCandidate {
                pc: kind,
                candidate: Some(IceCandidate {
                    candidate,
                    sdp_mid,
                    sdp_mline_index: Some(mline),
                }),
            });
            None
        });

        let events = self.events.clone();
        webrtc.connect_notify(Some("ice-gathering-state"), move |webrtc, _| {
            let state =
                webrtc.property::<gst_webrtc::WebRTCICEGatheringState>("ice-gathering-state");
            if state == gst_webrtc::WebRTCICEGatheringState::Complete {
                let _ = events.send(EngineEvent::LocalCandidate {
                    pc: kind,
                    candidate: None,
                });
            }
        });

        let events = self.events.clone();
        webrtc.connect_notify(Some("connection-state"), move |webrtc, _| {
            let state =
                webrtc.property::<gst_webrtc::WebRTCPeerConnectionState>("connection-state");
            let _ = events.send(EngineEvent::ConnectionState { pc: kind, state });
        });

        // Forward pipeline errors without needing a GLib main loop.
        let events = self.events.clone();
        let bus = pipeline
            .bus()
            .ok_or(Error::Setup("pipeline has no bus".into()))?;
        bus.set_sync_handler(move |_, msg| {
            match msg.view() {
                gst::MessageView::Error(err) => {
                    let message = format!(
                        "{}: {} ({})",
                        err.src()
                            .map(|s| s.path_string().to_string())
                            .unwrap_or_default(),
                        err.error(),
                        err.debug().unwrap_or_default()
                    );
                    tracing::warn!(?kind, %message, "media pipeline error");
                    let _ = events.send(EngineEvent::Error { pc: kind, message });
                }
                gst::MessageView::Warning(w) => {
                    tracing::debug!(?kind, warning = %w.error(), "media pipeline warning");
                }
                _ => {}
            }
            gst::BusSyncReply::Drop
        });
        Ok(())
    }
}

/// Build the `gst-launch` description of the publish pipeline.
fn publish_description(config: &EngineConfig) -> Result<String> {
    let mut desc = String::from("webrtcbin name=webrtc bundle-policy=max-bundle ");

    let audio_src = match &config.mic {
        MicSource::Auto => Some("autoaudiosrc".to_string()),
        MicSource::Test => Some("audiotestsrc is-live=true wave=ticks volume=0.3".to_string()),
        MicSource::None => None,
    };
    if let Some(src) = audio_src {
        // Echo cancellation is left to PipeWire's echo-cancel module (MEDIA.md §3b);
        // webrtcdsp adds noise suppression + AGC when available.
        let dsp = if config.mic == MicSource::Auto && has("webrtcdsp") {
            "webrtcdsp echo-cancel=false noise-suppression-level=moderate gain-control=true ! audioconvert ! "
        } else {
            ""
        };
        desc.push_str(&format!(
            "{src} ! queue ! audioconvert ! audioresample ! audio/x-raw,rate=48000,channels=1 ! \
             {dsp}volume name=mic_volume ! opusenc bitrate=32000 ! rtpopuspay pt=111 ! \
             application/x-rtp,media=audio,encoding-name=OPUS,payload=111 ! webrtc. "
        ));
    }

    let video_src = match &config.camera {
        CameraSource::Auto => Some("autovideosrc".to_string()),
        CameraSource::Device(path) => {
            Some(format!("v4l2src device={} ! decodebin", launch_quote(path)))
        }
        CameraSource::Test => Some("videotestsrc is-live=true pattern=ball".to_string()),
        CameraSource::None => None,
    };
    if let Some(src) = video_src {
        let encoder = video_encoder(config)?;
        desc.push_str(&format!(
            "{src} ! videoconvert ! videoscale ! videorate ! \
             video/x-raw,width=1280,height=720,framerate=30/1 ! tee name=vt \
             vt. ! queue name=preview_q leaky=downstream max-size-buffers=2 \
             vt. ! queue leaky=downstream max-size-buffers=2 ! valve name=cam_valve ! \
             videoconvert ! {encoder} ! webrtc. "
        ));
    }
    Ok(desc)
}

/// The encoder + payloader chain for the configured codec, with fallbacks.
fn video_encoder(config: &EngineConfig) -> Result<String> {
    let kbps = config.video_kbps;
    // rtph264pay derives profile-level-id from the SPS (x264's constrained
    // baseline is 42c01f); the SFU matches the SDP string 42e01f exactly, so
    // advertise that. Same profile, only the constraint_set2 flag differs.
    let h264_pay =
        "h264parse ! rtph264pay config-interval=-1 aggregate-mode=zero-latency pt=102 ! \
         capssetter caps=\"application/x-rtp,profile-level-id=(string)42e01f\" ! \
         application/x-rtp,media=video,encoding-name=H264,payload=102";
    let vp8 = || {
        format!(
            "vp8enc name=venc deadline=1 cpu-used=8 target-bitrate={} keyframe-max-dist=60 \
             error-resilient=partitions ! rtpvp8pay pt=96 picture-id-mode=15-bit ! \
             application/x-rtp,media=video,encoding-name=VP8,payload=96",
            kbps * 1000
        )
    };

    if config.codec == VideoCodec::Vp8 {
        return if has("vp8enc") {
            Ok(vp8())
        } else {
            Err(Error::Setup("vp8enc missing".into()))
        };
    }
    if config.hardware_encode {
        for hw in ["vah264lpenc", "vah264enc"] {
            if has(hw) {
                return Ok(format!(
                    "{hw} name=venc bitrate={kbps} key-int-max=60 ! \
                     video/x-h264,profile=constrained-baseline ! {h264_pay}"
                ));
            }
        }
    }
    if has("x264enc") {
        return Ok(format!(
            "x264enc name=venc tune=zerolatency speed-preset=ultrafast bitrate={kbps} \
             key-int-max=60 bframes=0 ! video/x-h264,profile=constrained-baseline ! {h264_pay}"
        ));
    }
    if has("openh264enc") {
        return Ok(format!(
            "openh264enc name=venc bitrate={} gop-size=60 ! {h264_pay}",
            kbps * 1000
        ));
    }
    if has("vp8enc") {
        tracing::warn!("no H.264 encoder found; falling back to VP8");
        return Ok(vp8());
    }
    Err(Error::Setup(
        "no video encoder (x264enc/openh264enc/vp8enc) installed".into(),
    ))
}

/// Decode a new remote pad and render it into an app-provided sink.
/// The decode chain behind one remote webrtcbin pad, by mid.
struct Chain {
    pad: gst::Pad,
    elements: Vec<gst::Element>,
}

type Chains = Arc<Mutex<std::collections::HashMap<String, Chain>>>;

/// Stop and remove a retired decode chain. Runs via `call_async`: this is
/// reached from webrtcbin's streaming thread, which also feeds the chain, so
/// stopping it inline could deadlock.
fn teardown(pipeline: &gst::Pipeline, chain: Chain) {
    pipeline.call_async(move |pipeline| {
        let Some(pipeline) = pipeline.downcast_ref::<gst::Pipeline>() else {
            return;
        };
        if let Some(peer) = chain.pad.peer() {
            let _ = chain.pad.unlink(&peer);
        }
        for e in &chain.elements {
            let _ = e.set_state(gst::State::Null);
        }
        let _ = pipeline.remove_many(&chain.elements);
    });
}

#[allow(clippy::too_many_arguments)]
fn link_remote(
    pipeline: &gst::Pipeline,
    pad: &gst::Pad,
    mid: String,
    chains: &Chains,
    video_sink: SinkFactory,
    audio_sink: Option<SinkFactory>,
    events: mpsc::UnboundedSender<EngineEvent>,
) -> Result<()> {
    let decodebin = make("decodebin")?;
    chains.lock().unwrap().insert(
        mid.clone(),
        Chain {
            pad: pad.clone(),
            elements: vec![decodebin.clone()],
        },
    );
    let chains = chains.clone();
    let chain_pad = pad.clone();
    let pipeline_weak = pipeline.downgrade();
    decodebin.connect_pad_added(move |_db, src| {
        let Some(pipeline) = pipeline_weak.upgrade() else {
            return;
        };
        let caps = src.current_caps().or_else(|| src.allowed_caps());
        let name = caps
            .as_ref()
            .and_then(|c| c.structure(0).map(|s| s.name().to_string()))
            .unwrap_or_default();
        let (kind, chain): (TrackKind, Vec<&str>) = if name.starts_with("video/") {
            (TrackKind::Video, vec!["queue", "videoconvert"])
        } else if name.starts_with("audio/") {
            (
                TrackKind::Audio,
                vec!["queue", "audioconvert", "audioresample"],
            )
        } else {
            return;
        };
        let sink = match (kind, &audio_sink) {
            (TrackKind::Video, _) => video_sink(TrackKind::Video),
            (TrackKind::Audio, Some(factory)) => factory(TrackKind::Audio),
            (TrackKind::Audio, None) => match make("autoaudiosink") {
                Ok(s) => s,
                Err(_) => return,
            },
        };
        let elements: Result<Vec<gst::Element>> = chain.iter().map(|f| make(f)).collect();
        let mut elements = match elements {
            Ok(e) => e,
            Err(err) => {
                let _ = events.send(EngineEvent::Error {
                    pc: PcKind::Subscribe,
                    message: format!("remote {kind:?} for mid {mid}: {err}"),
                });
                return;
            }
        };
        elements.push(sink.clone());
        if pipeline.add_many(&elements).is_err() || gst::Element::link_many(&elements).is_err() {
            let _ = events.send(EngineEvent::Error {
                pc: PcKind::Subscribe,
                message: format!("could not link remote {kind:?} for mid {mid}"),
            });
            return;
        }
        for e in &elements {
            let _ = e.sync_state_with_parent();
        }
        // Record them for teardown, unless the mid was already reassigned.
        match chains.lock().unwrap().get_mut(&mid) {
            Some(chain) if chain.pad == chain_pad => {
                chain.elements.extend(elements.iter().cloned())
            }
            _ => {
                let pipeline = pipeline.clone();
                teardown(
                    &pipeline,
                    Chain {
                        pad: chain_pad.clone(),
                        elements: elements.clone(),
                    },
                );
                return;
            }
        }
        let sinkpad = elements[0].static_pad("sink").expect("queue sink pad");
        if src.link(&sinkpad).is_err() {
            return;
        }
        let _ = events.send(EngineEvent::RemoteTrack {
            mid: mid.clone(),
            kind,
            sink,
        });
    });
    pipeline
        .add(&decodebin)
        .map_err(|e| Error::Setup(e.to_string()))?;
    decodebin
        .sync_state_with_parent()
        .map_err(|e| Error::Setup(e.to_string()))?;
    let sinkpad = decodebin.static_pad("sink").expect("decodebin sink pad");
    pad.link(&sinkpad)
        .map_err(|e| Error::Setup(format!("link remote pad: {e:?}")))?;
    Ok(())
}

fn make(factory: &str) -> Result<gst::Element> {
    gst::ElementFactory::make(factory)
        .build()
        .map_err(|_| Error::Setup(format!("GStreamer element {factory} is missing")))
}

/// Quote a value for a `gst-launch` description (`"..."`, `\` and `"` escaped).
fn launch_quote(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

fn has(factory: &str) -> bool {
    gst::ElementFactory::find(factory).is_some()
}

fn apply_ice_servers(webrtc: &gst::Element, servers: &[IceServer]) {
    for (server, url) in servers
        .iter()
        .flat_map(|s| s.urls.iter().map(move |u| (s, u.as_str())))
    {
        if let Some(rest) = url.strip_prefix("stun:") {
            webrtc.set_property(
                "stun-server",
                format!("stun://{}", rest.trim_start_matches("//")),
            );
        } else if let Some((scheme, rest)) = url.split_once(':') {
            if scheme != "turn" && scheme != "turns" {
                continue;
            }
            let rest = rest.trim_start_matches("//");
            let creds = match (&server.username, &server.credential) {
                (Some(u), Some(p)) => format!("{}:{}@", uri_escape(u), uri_escape(p)),
                _ => String::new(),
            };
            let uri = format!("{scheme}://{creds}{rest}");
            if !webrtc.emit_by_name::<bool>("add-turn-server", &[&uri]) {
                // Log the host part only: the URI carries credentials.
                tracing::warn!(server = %rest, "webrtcbin rejected TURN server");
            }
        }
    }
}

/// Percent-encode a TURN credential for the `turn://user:pass@host` URI.
fn uri_escape(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn transceiver_mid(webrtc: &gst::Element, mline: u32) -> Option<String> {
    let t = webrtc.emit_by_name::<Option<gst_webrtc::WebRTCRTPTransceiver>>(
        "get-transceiver",
        &[&(mline as i32)],
    )?;
    t.property::<Option<String>>("mid")
}

fn transceivers(
    webrtc: &gst::Element,
) -> impl Iterator<Item = gst_webrtc::WebRTCRTPTransceiver> + '_ {
    (0..).map_while(|i: i32| {
        webrtc.emit_by_name::<Option<gst_webrtc::WebRTCRTPTransceiver>>("get-transceiver", &[&i])
    })
}

fn mline_for_mid(webrtc: &gst::Element, mid: &str) -> Option<u32> {
    transceivers(webrtc).find_map(|t| {
        (t.property::<Option<String>>("mid").as_deref() == Some(mid))
            .then(|| t.property::<u32>("mlineindex"))
    })
}

/// Ask the video encoder for a keyframe (after the camera resumes).
fn request_keyframe(pipeline: &gst::Pipeline) {
    if let Some(pad) = pipeline.by_name("venc").and_then(|e| e.static_pad("src")) {
        let event = gst_video::UpstreamForceKeyUnitEvent::builder()
            .all_headers(true)
            .build();
        pad.send_event(event);
    }
}

/// Wait (bounded) until every webrtcbin sink pad knows its caps, so the offer
/// carries real codec parameters (profile-level-id, packetization-mode).
async fn wait_for_sink_caps(webrtc: &gst::Element, closed: &AtomicBool) {
    let deadline = tokio::time::Instant::now() + CAPS_TIMEOUT;
    loop {
        if closed.load(Ordering::SeqCst) {
            return; // the caller's fence check fails the operation
        }
        let ready = webrtc
            .sink_pads()
            .iter()
            .all(|p| p.current_caps().is_some());
        if ready || tokio::time::Instant::now() >= deadline {
            if !ready {
                tracing::warn!("offering before all publish caps were negotiated");
            }
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn parse_description(
    kind: gst_webrtc::WebRTCSDPType,
    sdp: &str,
) -> Result<gst_webrtc::WebRTCSessionDescription> {
    let msg = gst_sdp::SDPMessage::parse_buffer(sdp.as_bytes())
        .map_err(|e| Error::Sdp(format!("unparseable SDP: {e}")))?;
    Ok(gst_webrtc::WebRTCSessionDescription::new(kind, msg))
}

fn sdp_text(desc: &gst_webrtc::WebRTCSessionDescription) -> Result<String> {
    desc.sdp()
        .as_text()
        .map_err(|e| Error::Sdp(format!("serialize SDP: {e}")))
}

/// Run `create-offer` / `create-answer` and return the description.
async fn create_description(
    webrtc: &gst::Element,
    signal: &str,
    field: &'static str,
) -> Result<gst_webrtc::WebRTCSessionDescription> {
    let (tx, rx) = oneshot::channel();
    let promise = gst::Promise::with_change_func(move |reply| {
        let result = match reply {
            Ok(Some(s)) => match s.get::<gst_webrtc::WebRTCSessionDescription>(field) {
                Ok(desc) => Ok(desc),
                Err(_) => Err(Error::Sdp(promise_error(s, field))),
            },
            Ok(None) => Err(Error::Sdp(format!("{field}: empty reply"))),
            Err(e) => Err(Error::Sdp(format!("{field}: {e:?}"))),
        };
        let _ = tx.send(result);
    });
    webrtc.emit_by_name::<()>(signal, &[&None::<gst::Structure>, &promise]);
    rx.await
        .map_err(|_| Error::Sdp(format!("{field}: promise dropped")))?
}

/// Run `set-local-description` / `set-remote-description`, surfacing errors.
async fn set_description(
    webrtc: &gst::Element,
    signal: &str,
    desc: &gst_webrtc::WebRTCSessionDescription,
) -> Result<()> {
    let (tx, rx) = oneshot::channel();
    let promise = gst::Promise::with_change_func(move |reply| {
        let result = match reply {
            Ok(Some(s)) if s.has_field("error") => Err(Error::Sdp(promise_error(s, "error"))),
            Ok(_) => Ok(()),
            Err(e) => Err(Error::Sdp(format!("{e:?}"))),
        };
        let _ = tx.send(result);
    });
    webrtc.emit_by_name::<()>(signal, &[desc, &promise]);
    rx.await.map_err(|_| Error::Sdp("promise dropped".into()))?
}

fn promise_error(s: &gst::StructureRef, context: &str) -> String {
    match s.get::<gst::glib::Error>("error") {
        Ok(e) => format!("{context}: {}", e.message()),
        Err(_) => format!("{context}: unexpected reply {s}"),
    }
}

/// Core drives the engine through this; the inherent methods do the work.
#[async_trait::async_trait]
impl brook_core::MediaEngine for GstEngine {
    async fn create_publish_offer(&self) -> std::result::Result<String, brook_core::EngineError> {
        Ok(GstEngine::create_publish_offer(self).await?)
    }

    async fn apply_publish_answer(
        &self,
        sdp: String,
    ) -> std::result::Result<(), brook_core::EngineError> {
        Ok(GstEngine::apply_publish_answer(self, &sdp).await?)
    }

    async fn apply_subscribe_offer(
        &self,
        sdp: String,
        streams: Vec<SubStream>,
    ) -> std::result::Result<String, brook_core::EngineError> {
        Ok(GstEngine::apply_subscribe_offer(self, &sdp, streams).await?)
    }

    fn add_remote_candidate(
        &self,
        pc: PcKind,
        candidate: Option<IceCandidate>,
    ) -> std::result::Result<(), brook_core::EngineError> {
        Ok(GstEngine::add_remote_candidate(
            self,
            pc,
            candidate.as_ref(),
        )?)
    }

    fn set_local_media(
        &self,
        audio: bool,
        video: bool,
    ) -> std::result::Result<(), brook_core::EngineError> {
        Ok(GstEngine::set_local_media(self, audio, video)?)
    }

    fn set_ice_servers(&self, servers: Vec<IceServer>) {
        GstEngine::set_ice_servers(self, servers);
    }

    async fn close(&self) {
        GstEngine::close(self);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(camera: CameraSource) -> EngineConfig {
        EngineConfig {
            camera,
            mic: MicSource::None,
            codec: VideoCodec::Vp8,
            hardware_encode: false,
            video_kbps: 1500,
            ice_servers: vec![],
            video_sink: Arc::new(|_| gst::ElementFactory::make("fakesink").build().unwrap()),
            audio_sink: None,
        }
    }

    /// A device path with spaces or quotes must not break (or inject into)
    /// the pipeline description.
    #[test]
    fn device_path_is_quoted() {
        gst::init().unwrap();
        let path = r#"/dev/my "odd" cam ! fakesink"#;
        let desc = publish_description(&config(CameraSource::Device(path.into()))).unwrap();
        let pipeline = gst::parse::launch(&desc)
            .expect("description parses")
            .downcast::<gst::Pipeline>()
            .unwrap();
        let src = pipeline
            .iterate_elements()
            .into_iter()
            .flatten()
            .find(|e| e.factory().map(|f| f.name() == "v4l2src").unwrap_or(false))
            .expect("v4l2src present");
        assert_eq!(src.property::<String>("device"), path);
    }
}
