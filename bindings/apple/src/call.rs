//! Calls over the FFI: the platform engine is implemented in Swift (`FfiMediaEngine`, a
//! foreign async trait) and adapted to core's `MediaEngine`; the call handle and its state
//! are exposed back to Swift. See docs/superpowers/specs/2026-09-24-apple-call-engine-design.md.

use std::sync::Arc;

use async_trait::async_trait;
use brook_core::{
    CallHandle, CallState, CallStatus, EndReason, EngineError, IceCandidate, IceServer, MediaKind,
    MediaSource, Participant, PcKind, PublishOffer, SubStream, TrackLabel,
};

use crate::listener::{subscribe_watch, Subscription};
use crate::runtime::runtime;
use crate::types::LoginError;

// ---- FFI mirrors of core's call types ----

/// Which PeerConnection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum FfiPcKind {
    Publish,
    Subscribe,
}

/// An ICE candidate (`RTCIceCandidate` fields).
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FfiIceCandidate {
    pub candidate: String,
    pub sdp_mid: Option<String>,
    pub sdp_mline_index: Option<u32>,
}

/// A STUN/TURN server (`RTCIceServer` fields).
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FfiIceServer {
    pub urls: Vec<String>,
    pub username: Option<String>,
    pub credential: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum FfiMediaKind {
    Audio,
    Video,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum FfiMediaSource {
    Mic,
    Camera,
    Screen,
    Unknown,
}

/// What one publish m-line carries (`call.publish` `tracks`, PROTOCOL.md §3.3).
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FfiTrackLabel {
    pub mid: String,
    pub kind: FfiMediaKind,
    pub source: FfiMediaSource,
}

/// A publish offer with one label per audio/video m-line (inactive ones included).
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FfiPublishOffer {
    pub sdp: String,
    pub tracks: Vec<FfiTrackLabel>,
}

/// Which participant a subscribe-PC `mid` belongs to.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FfiSubStream {
    pub mid: String,
    pub participant_id: String,
    pub kind: FfiMediaKind,
    pub source: FfiMediaSource,
}

/// Someone else in the call.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FfiParticipant {
    pub participant_id: String,
    pub user_id: String,
    pub display_name: String,
    pub audio: bool,
    pub video: bool,
}

/// Why a call ended.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum FfiEndReason {
    Left,
    SfuRestart,
    Removed,
    Replaced,
    Expired,
    SessionChanged,
    EngineFailed {
        message: String,
    },
    Server {
        code: String,
    },
    /// A reason this build does not know yet.
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum FfiCallStatus {
    Joining,
    Connected,
    Reconnecting,
    Ended { reason: FfiEndReason },
}

/// A snapshot of the call.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FfiCallState {
    pub status: FfiCallStatus,
    pub call_id: Option<String>,
    pub self_participant: Option<String>,
    pub participants: Vec<FfiParticipant>,
}

/// An engine failure reported by Swift.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error, uniffi::Error)]
pub enum FfiEngineError {
    #[error("{message}")]
    Failed { message: String },
}

// ---- conversions ----

impl From<PcKind> for FfiPcKind {
    fn from(p: PcKind) -> Self {
        match p {
            PcKind::Publish => Self::Publish,
            PcKind::Subscribe => Self::Subscribe,
        }
    }
}
impl From<FfiPcKind> for PcKind {
    fn from(p: FfiPcKind) -> Self {
        match p {
            FfiPcKind::Publish => Self::Publish,
            FfiPcKind::Subscribe => Self::Subscribe,
        }
    }
}
impl From<IceCandidate> for FfiIceCandidate {
    fn from(c: IceCandidate) -> Self {
        Self {
            candidate: c.candidate,
            sdp_mid: c.sdp_mid,
            sdp_mline_index: c.sdp_mline_index,
        }
    }
}
impl From<FfiIceCandidate> for IceCandidate {
    fn from(c: FfiIceCandidate) -> Self {
        Self {
            candidate: c.candidate,
            sdp_mid: c.sdp_mid,
            sdp_mline_index: c.sdp_mline_index,
        }
    }
}
impl From<IceServer> for FfiIceServer {
    fn from(s: IceServer) -> Self {
        Self {
            urls: s.urls,
            username: s.username,
            credential: s.credential,
        }
    }
}
impl From<MediaKind> for FfiMediaKind {
    fn from(k: MediaKind) -> Self {
        match k {
            MediaKind::Audio => Self::Audio,
            MediaKind::Video => Self::Video,
            _ => Self::Unknown,
        }
    }
}
impl From<MediaSource> for FfiMediaSource {
    fn from(s: MediaSource) -> Self {
        match s {
            MediaSource::Mic => Self::Mic,
            MediaSource::Camera => Self::Camera,
            MediaSource::Screen => Self::Screen,
            _ => Self::Unknown,
        }
    }
}
impl From<FfiMediaKind> for MediaKind {
    fn from(k: FfiMediaKind) -> Self {
        match k {
            FfiMediaKind::Audio => Self::Audio,
            FfiMediaKind::Video => Self::Video,
            FfiMediaKind::Unknown => Self::Unknown,
        }
    }
}
impl From<FfiMediaSource> for MediaSource {
    fn from(s: FfiMediaSource) -> Self {
        match s {
            FfiMediaSource::Mic => Self::Mic,
            FfiMediaSource::Camera => Self::Camera,
            FfiMediaSource::Screen => Self::Screen,
            FfiMediaSource::Unknown => Self::Unknown,
        }
    }
}
impl From<FfiPublishOffer> for PublishOffer {
    fn from(o: FfiPublishOffer) -> Self {
        Self {
            sdp: o.sdp,
            tracks: o
                .tracks
                .into_iter()
                .map(|t| TrackLabel {
                    mid: t.mid,
                    kind: t.kind.into(),
                    source: t.source.into(),
                })
                .collect(),
        }
    }
}
impl From<SubStream> for FfiSubStream {
    fn from(s: SubStream) -> Self {
        Self {
            mid: s.mid,
            participant_id: s.participant_id,
            kind: s.kind.into(),
            source: s.source.into(),
        }
    }
}
impl From<Participant> for FfiParticipant {
    fn from(p: Participant) -> Self {
        Self {
            participant_id: p.participant_id,
            user_id: p.user_id,
            display_name: p.display_name,
            audio: p.audio,
            video: p.video,
        }
    }
}
impl From<EndReason> for FfiEndReason {
    fn from(r: EndReason) -> Self {
        match r {
            EndReason::Left => Self::Left,
            EndReason::SfuRestart => Self::SfuRestart,
            EndReason::Removed => Self::Removed,
            EndReason::Replaced => Self::Replaced,
            EndReason::Expired => Self::Expired,
            EndReason::SessionChanged => Self::SessionChanged,
            EndReason::EngineFailed(message) => Self::EngineFailed { message },
            EndReason::Server(code) => Self::Server { code },
            _ => Self::Unknown,
        }
    }
}
impl From<CallState> for FfiCallState {
    fn from(s: CallState) -> Self {
        Self {
            status: match s.status {
                CallStatus::Joining => FfiCallStatus::Joining,
                CallStatus::Connected => FfiCallStatus::Connected,
                CallStatus::Reconnecting => FfiCallStatus::Reconnecting,
                CallStatus::Ended(reason) => FfiCallStatus::Ended {
                    reason: reason.into(),
                },
            },
            call_id: s.call_id,
            self_participant: s.self_participant,
            participants: s.participants.into_iter().map(Into::into).collect(),
        }
    }
}
impl From<FfiEngineError> for EngineError {
    fn from(e: FfiEngineError) -> Self {
        match e {
            FfiEngineError::Failed { message } => EngineError(message),
        }
    }
}

// ---- the engine, implemented in Swift ----

/// The platform media engine (libwebrtc on Apple), implemented in Swift. Same contract as
/// core's `MediaEngine`: async operations finish on local WebRTC work alone; the sync ones
/// never block (they are called on Rust worker threads, inline in the call task); `close()`
/// fences every operation still running.
#[uniffi::export(with_foreign)]
#[async_trait]
pub trait FfiMediaEngine: Send + Sync {
    /// The publish offer (set as the publish PC's local description) with a label for every
    /// audio/video m-line: `screen` for a screen share's, including a stopped one.
    async fn create_labelled_offer(&self) -> Result<FfiPublishOffer, FfiEngineError>;
    async fn apply_publish_answer(&self, sdp: String) -> Result<(), FfiEngineError>;
    async fn apply_subscribe_offer(
        &self,
        sdp: String,
        streams: Vec<FfiSubStream>,
    ) -> Result<String, FfiEngineError>;
    fn add_remote_candidate(
        &self,
        pc: FfiPcKind,
        candidate: Option<FfiIceCandidate>,
    ) -> Result<(), FfiEngineError>;
    fn set_local_media(&self, audio: bool, video: bool) -> Result<(), FfiEngineError>;
    fn set_ice_servers(&self, servers: Vec<FfiIceServer>);
    async fn close(&self);
}

/// Adapts the Swift engine to core's trait. Forwards every method — including
/// `set_ice_servers`, whose core default would silently do nothing.
pub(crate) struct EngineAdapter(pub(crate) Arc<dyn FfiMediaEngine>);

#[async_trait]
impl brook_core::MediaEngine for EngineAdapter {
    /// Core always calls the labelled one; this exists for the trait and never loses labels
    /// because core does not use it.
    async fn create_publish_offer(&self) -> Result<String, EngineError> {
        Ok(self.0.create_labelled_offer().await?.sdp)
    }
    async fn create_labelled_offer(&self) -> Result<PublishOffer, EngineError> {
        Ok(self.0.create_labelled_offer().await?.into())
    }
    async fn apply_publish_answer(&self, sdp: String) -> Result<(), EngineError> {
        Ok(self.0.apply_publish_answer(sdp).await?)
    }
    async fn apply_subscribe_offer(
        &self,
        sdp: String,
        streams: Vec<SubStream>,
    ) -> Result<String, EngineError> {
        let streams = streams.into_iter().map(Into::into).collect();
        Ok(self.0.apply_subscribe_offer(sdp, streams).await?)
    }
    fn add_remote_candidate(
        &self,
        pc: PcKind,
        candidate: Option<IceCandidate>,
    ) -> Result<(), EngineError> {
        Ok(self
            .0
            .add_remote_candidate(pc.into(), candidate.map(Into::into))?)
    }
    fn set_local_media(&self, audio: bool, video: bool) -> Result<(), EngineError> {
        Ok(self.0.set_local_media(audio, video)?)
    }
    fn set_ice_servers(&self, servers: Vec<IceServer>) {
        self.0
            .set_ice_servers(servers.into_iter().map(Into::into).collect());
    }
    async fn close(&self) {
        self.0.close().await;
    }
}

// ---- listeners ----

/// Receives call-state snapshots (latest state wins; see `AuthStateListener`).
#[uniffi::export(with_foreign)]
pub trait CallStateListener: Send + Sync {
    fn on_state(&self, state: FfiCallState);
}

/// Realtime events the Apple UI uses today.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum FfiServerEvent {
    /// The socket authenticated; calls can be joined.
    Ready,
    /// A message arrived.
    MessageNew { message: crate::offline::FfiMessage },
    /// A message was edited, or one of its files deleted: replace it whole.
    MessageUpdate { message: crate::offline::FfiMessage },
    /// A message was deleted: show it as deleted.
    MessageDelete {
        channel_id: String,
        message_id: String,
    },
    /// Events were missed (a slow listener): reload what's shown.
    Resync,
    /// A call started, changed size, or ended (`call_id` none) in a channel.
    ChannelCall {
        channel_id: String,
        call_id: Option<String>,
        participant_count: u32,
    },
    /// A channel you're in changed (renamed, members joined or left, archived): replace
    /// its row. Without local data this is the only way the list hears of it.
    ChannelUpdate { channel: crate::types::FfiChannel },
    /// You're no longer in this channel (it was deleted, you left, or you were removed).
    ChannelDelete { channel_id: String },
}

#[uniffi::export(with_foreign)]
pub trait ServerEventListener: Send + Sync {
    fn on_event(&self, event: FfiServerEvent);
}

// ---- the call handle ----

/// A live call. Dropping the last Swift reference leaves the call.
#[derive(uniffi::Object)]
pub struct FfiCallHandle {
    inner: Arc<CallHandle>,
}

impl FfiCallHandle {
    pub(crate) fn new(inner: Arc<CallHandle>) -> Arc<Self> {
        Arc::new(Self { inner })
    }
}

#[uniffi::export]
impl FfiCallHandle {
    /// Observe the call. Latest state wins; cancel (or drop) the subscription to stop.
    pub fn subscribe_state(&self, listener: Arc<dyn CallStateListener>) -> Arc<Subscription> {
        subscribe_watch(self.inner.state(), move |state: CallState| {
            listener.on_state(state.into())
        })
    }

    /// A local candidate from the engine (`None`: gathering complete). Non-blocking.
    pub fn local_candidate(&self, pc: FfiPcKind, candidate: Option<FfiIceCandidate>) {
        self.inner
            .local_candidate(pc.into(), candidate.map(Into::into));
    }

    /// The engine failed unrecoverably: end the call. Non-blocking.
    pub fn engine_failed(&self, message: String) {
        self.inner.engine_failed(message);
    }

    pub async fn set_media(&self, audio: bool, video: bool) -> Result<(), LoginError> {
        let inner = Arc::clone(&self.inner);
        run(async move { inner.set_media(audio, video).await }).await
    }

    pub async fn republish(&self) -> Result<(), LoginError> {
        let inner = Arc::clone(&self.inner);
        run(async move { inner.republish().await }).await
    }

    /// Leave; resolves once the server confirmed (bounded). Media stops at once.
    pub async fn leave(&self) -> Result<(), LoginError> {
        let inner = Arc::clone(&self.inner);
        run(async move { inner.leave().await }).await
    }
}

/// Run a core future on the owned runtime (Swift polls on its own executor).
pub(crate) async fn run<T: Send + 'static>(
    fut: impl std::future::Future<Output = brook_core::Result<T>> + Send + 'static,
) -> Result<T, LoginError> {
    match runtime().spawn(fut).await {
        Ok(result) => Ok(result?),
        Err(_) => Err(LoginError::UnexpectedResponse),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;
    use std::time::Duration;

    use brook_core::test_support::TestServer;
    use brook_core::MediaEngine as _;
    use serde_json::json;

    use super::*;
    use crate::FfiBrookClient;

    /// Records every call with its arguments; the offer it produces is a sentinel.
    #[derive(Default)]
    struct FakeEngine {
        log: Mutex<Vec<String>>,
        closes: AtomicUsize,
        fail_media: bool,
    }

    impl FakeEngine {
        fn log(&self) -> Vec<String> {
            self.log.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl FfiMediaEngine for FakeEngine {
        async fn create_labelled_offer(&self) -> Result<FfiPublishOffer, FfiEngineError> {
            self.log
                .lock()
                .unwrap()
                .push("create_labelled_offer".into());
            Ok(FfiPublishOffer {
                sdp: "offer-from-swift".into(),
                tracks: vec![
                    FfiTrackLabel {
                        mid: "a0".into(),
                        kind: FfiMediaKind::Audio,
                        source: FfiMediaSource::Mic,
                    },
                    FfiTrackLabel {
                        mid: "s2".into(),
                        kind: FfiMediaKind::Video,
                        source: FfiMediaSource::Screen,
                    },
                ],
            })
        }
        async fn apply_publish_answer(&self, sdp: String) -> Result<(), FfiEngineError> {
            self.log
                .lock()
                .unwrap()
                .push(format!("apply_publish_answer:{sdp}"));
            Ok(())
        }
        async fn apply_subscribe_offer(
            &self,
            sdp: String,
            streams: Vec<FfiSubStream>,
        ) -> Result<String, FfiEngineError> {
            let s: Vec<String> = streams
                .iter()
                .map(|s| format!("{}>{}:{:?}:{:?}", s.mid, s.participant_id, s.kind, s.source))
                .collect();
            self.log
                .lock()
                .unwrap()
                .push(format!("apply_subscribe_offer:{sdp}:{}", s.join(",")));
            Ok("answer".into())
        }
        fn add_remote_candidate(
            &self,
            pc: FfiPcKind,
            c: Option<FfiIceCandidate>,
        ) -> Result<(), FfiEngineError> {
            let what =
                c.map(|c| format!("{}|{:?}|{:?}", c.candidate, c.sdp_mid, c.sdp_mline_index));
            self.log
                .lock()
                .unwrap()
                .push(format!("remote:{pc:?}:{what:?}"));
            Ok(())
        }
        fn set_local_media(&self, audio: bool, video: bool) -> Result<(), FfiEngineError> {
            if self.fail_media {
                return Err(FfiEngineError::Failed {
                    message: "no camera".into(),
                });
            }
            self.log
                .lock()
                .unwrap()
                .push(format!("media:{audio}:{video}"));
            Ok(())
        }
        fn set_ice_servers(&self, servers: Vec<FfiIceServer>) {
            let s: Vec<String> = servers
                .iter()
                .map(|s| format!("{}|{:?}|{:?}", s.urls.join(";"), s.username, s.credential))
                .collect();
            self.log
                .lock()
                .unwrap()
                .push(format!("ice_servers:{}", s.join(",")));
        }
        async fn close(&self) {
            self.closes.fetch_add(1, Ordering::SeqCst);
            self.log.lock().unwrap().push("close".into());
        }
    }

    /// Every method reaches the Swift object with its field values intact and unswapped.
    #[tokio::test]
    async fn adapter_forwards_every_method_with_exact_fields() {
        let fake = Arc::new(FakeEngine::default());
        let adapter = EngineAdapter(fake.clone());
        let offer = adapter.create_labelled_offer().await.unwrap();
        assert_eq!(offer.sdp, "offer-from-swift");
        assert_eq!(
            offer
                .tracks
                .iter()
                .map(|t| (t.mid.as_str(), t.kind, t.source))
                .collect::<Vec<_>>(),
            [
                ("a0", MediaKind::Audio, MediaSource::Mic),
                ("s2", MediaKind::Video, MediaSource::Screen)
            ]
        );
        adapter.apply_publish_answer("ans-1".into()).await.unwrap();
        adapter
            .apply_subscribe_offer(
                "off-1".into(),
                vec![SubStream {
                    mid: "m7".into(),
                    participant_id: "p9".into(),
                    kind: MediaKind::Video,
                    source: MediaSource::Camera,
                }],
            )
            .await
            .unwrap();
        adapter
            .add_remote_candidate(
                PcKind::Subscribe,
                Some(IceCandidate {
                    candidate: "cand-A".into(),
                    sdp_mid: Some("mid-B".into()),
                    sdp_mline_index: Some(3),
                }),
            )
            .unwrap();
        adapter.add_remote_candidate(PcKind::Publish, None).unwrap();
        adapter.set_local_media(false, true).unwrap();
        adapter.set_ice_servers(vec![IceServer {
            urls: vec!["turn:t.example".into()],
            username: Some("u-1".into()),
            credential: Some("c-1".into()),
        }]);
        adapter.close().await;
        assert_eq!(
            fake.log(),
            [
                "create_labelled_offer",
                "apply_publish_answer:ans-1",
                "apply_subscribe_offer:off-1:m7>p9:Video:Camera",
                "remote:Subscribe:Some(\"cand-A|Some(\\\"mid-B\\\")|Some(3)\")",
                "remote:Publish:None",
                "media:false:true",
                "ice_servers:turn:t.example|Some(\"u-1\")|Some(\"c-1\")",
                "close",
            ]
        );
    }

    #[tokio::test]
    async fn engine_errors_map_to_core_errors() {
        let fake = Arc::new(FakeEngine {
            fail_media: true,
            ..Default::default()
        });
        let err = EngineAdapter(fake).set_local_media(true, true).unwrap_err();
        assert_eq!(err, EngineError("no camera".into()));
    }

    #[test]
    fn unknown_kind_and_source_map_to_unknown() {
        let s: SubStream = serde_json::from_value(json!({
            "mid": "1", "participant_id": "p", "kind": "hologram", "source": "screen2"
        }))
        .unwrap();
        let f: FfiSubStream = s.into();
        assert_eq!(
            (f.kind, f.source),
            (FfiMediaKind::Unknown, FfiMediaSource::Unknown)
        );
    }

    /// Through the real client and a scripted server: the Swift engine's offer is awaited by
    /// core and reaches the server (not merely `Connected`, which core sets first). Then
    /// dropping the last handle leaves the call and closes the engine.
    #[tokio::test]
    async fn join_publishes_the_swift_offer_and_dropping_the_handle_leaves() {
        let mut server = TestServer::start().await;
        let client = FfiBrookClient::new(server.base.clone(), false).unwrap();
        client.login("mac".into(), "pw".into()).await.unwrap();
        client.start_realtime().await.unwrap();
        let mut peer = server.accept().await;
        peer.accept_auth().await;
        tokio::time::sleep(Duration::from_millis(100)).await; // ready observed by the client

        let fake = Arc::new(FakeEngine::default());
        let engine: Arc<dyn FfiMediaEngine> = fake.clone();
        let join = tokio::spawn({
            let client = client.clone();
            async move { client.join_call("ch".into(), engine, true).await }
        });
        let j = peer.recv().await;
        assert_eq!(j["type"], "call.join");
        peer.send(json!({ "type": "call.joined", "re": j["id"], "data": {
            "call_id": "k1", "channel_id": "ch",
            "self": { "participant_id": "me", "resume_token": "t1" },
            "participants": [], "ice_servers": [{ "urls": ["stun:s.example"] }] }}))
            .await;
        let handle = join.await.unwrap().unwrap();

        let publish = peer.recv().await;
        assert_eq!(publish["type"], "call.publish");
        assert_eq!(publish["data"]["sdp"], "offer-from-swift");
        // Swift's labels reach the wire as given (a screen stays a screen).
        assert_eq!(
            publish["data"]["tracks"],
            json!([
                { "mid": "a0", "kind": "audio", "source": "mic" },
                { "mid": "s2", "kind": "video", "source": "screen" }
            ])
        );
        assert!(fake
            .log()
            .iter()
            .any(|l| l.starts_with("ice_servers:stun:s.example")));

        drop(handle);
        let leave = peer.recv().await;
        assert_eq!(leave["type"], "call.leave");
        for _ in 0..200 {
            if fake.closes.load(Ordering::SeqCst) == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(fake.closes.load(Ordering::SeqCst), 1);
    }
}
