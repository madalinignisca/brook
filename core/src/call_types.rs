//! Public call types: the wire vocabulary of PROTOCOL.md §3 and the engine contract.
//!
//! Serde names are the wire names exactly (`sdpMid`, `sdpMLineIndex`, lowercase kinds).
//! Kinds and sources are `#[non_exhaustive]` with an `Unknown` fallback, so a later
//! additive value (e.g. a new `source`) does not break older clients.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// Which of a participant's two PeerConnections.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PcKind {
    /// Sendonly: our mic/camera. The client offers.
    Publish,
    /// Recvonly: every remote stream. The server offers.
    Subscribe,
}

/// An ICE candidate as WebRTC's `RTCIceCandidate.toJSON()` produces it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IceCandidate {
    /// The `candidate:` line.
    pub candidate: String,
    /// The media section's mid, when known.
    #[serde(rename = "sdpMid", default)]
    pub sdp_mid: Option<String>,
    /// The media section's index, when known.
    #[serde(rename = "sdpMLineIndex", default)]
    pub sdp_mline_index: Option<u32>,
}

/// STUN/TURN server, shaped like `RTCIceServer`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IceServer {
    /// `stun:` / `turn:` / `turns:` URLs.
    pub urls: Vec<String>,
    /// TURN username, if any.
    #[serde(default)]
    pub username: Option<String>,
    /// TURN credential, if any.
    #[serde(default)]
    pub credential: Option<String>,
}

/// Audio or video.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum MediaKind {
    /// Audio.
    Audio,
    /// Video.
    Video,
    /// A kind this client does not know yet.
    #[serde(other)]
    Unknown,
}

/// Where a stream comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum MediaSource {
    /// Microphone.
    Mic,
    /// Camera.
    Camera,
    /// Screen share.
    Screen,
    /// A source this client does not know yet.
    #[serde(other)]
    Unknown,
}

/// One remote stream in the subscribe PC: which participant a `mid` belongs to.
/// Mids are per receiver, so this travels with every subscribe offer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubStream {
    /// The transceiver mid in *our* subscribe PC.
    pub mid: String,
    /// Who sends it.
    pub participant_id: String,
    /// Audio or video.
    pub kind: MediaKind,
    /// Mic, camera, …
    pub source: MediaSource,
}

/// What a participant sends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Publishing {
    /// Audio or video.
    pub kind: MediaKind,
    /// Mic, camera, …
    pub source: MediaSource,
}

/// Someone in the call (other than us).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Participant {
    /// Stable for this participant's time in the call.
    pub participant_id: String,
    /// The Brook user.
    pub user_id: String,
    /// For tile labels.
    pub display_name: String,
    /// Mic on (as the participant sees it).
    pub audio: bool,
    /// Camera on.
    pub video: bool,
    /// What they send.
    #[serde(default)]
    pub publishing: Vec<Publishing>,
}

/// A failure inside the platform media engine.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("media engine: {0}")]
pub struct EngineError(pub String);

/// What one publish m-line carries, sent as `tracks` on `call.publish` (PROTOCOL.md §3.3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TrackLabel {
    /// The m-line's mid.
    pub mid: String,
    /// Audio or video.
    pub kind: MediaKind,
    /// Mic, camera or screen.
    pub source: MediaSource,
}

/// A publish offer with a label for its m-lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishOffer {
    /// The offer SDP (already the publish PC's local description).
    pub sdp: String,
    /// One label per audio/video m-line, inactive ones included.
    pub tracks: Vec<TrackLabel>,
}

/// The labels an engine without screen share means: every audio m-line is the mic, every
/// video m-line the camera. Inactive m-lines are labelled too: the contract requires active
/// ones and allows the rest, and labelling all of them never needs to know which is which.
/// An m-line without `a=mid` gets its position, as the server does.
pub fn default_labels(sdp: &str) -> Vec<TrackLabel> {
    let mut out: Vec<TrackLabel> = Vec::new();
    let mut current: Option<(usize, MediaKind)> = None;
    let mut mid: Option<String> = None;
    let mut position = 0usize;
    let mut flush = |current: &mut Option<(usize, MediaKind)>, mid: &mut Option<String>| {
        if let Some((pos, kind)) = current.take() {
            let source = if kind == MediaKind::Audio {
                MediaSource::Mic
            } else {
                MediaSource::Camera
            };
            let mid = mid.take().unwrap_or_else(|| pos.to_string());
            out.push(TrackLabel { mid, kind, source });
        }
        *mid = None;
    };
    for line in sdp.lines().map(str::trim) {
        if let Some(rest) = line.strip_prefix("m=") {
            flush(&mut current, &mut mid);
            let kind = match rest.split_whitespace().next() {
                Some("audio") => Some(MediaKind::Audio),
                Some("video") => Some(MediaKind::Video),
                _ => None,
            };
            current = kind.map(|k| (position, k));
            position += 1;
        } else if let Some(m) = line.strip_prefix("a=mid:") {
            if current.is_some() {
                mid = Some(m.to_string());
            }
        }
    }
    flush(&mut current, &mut mid);
    out
}

/// The platform's WebRTC stack (GStreamer on Linux, libwebrtc on Apple), driven by core's
/// call task. Core owns signaling; the engine owns capture, codecs, PeerConnections and
/// rendering.
///
/// The async operations must complete on local WebRTC work alone (setting descriptions,
/// creating an answer, a bounded capture start) and never wait for ICE connectivity or for
/// anything core delivers. Core spawns them and keeps servicing signaling meanwhile; at
/// most one description operation per PeerConnection is in flight.
#[async_trait]
pub trait MediaEngine: Send + Sync {
    /// Create a sendonly offer for the publish PC and set it as its local description.
    /// Called again to renegotiate (e.g. camera added later).
    async fn create_publish_offer(&self) -> Result<String, EngineError>;
    /// The publish offer with its m-line labels; what core sends. The default labels every
    /// audio m-line `mic` and every video m-line `camera`: an engine that shares its screen
    /// overrides this and labels that m-line `screen`, from the same call that made the
    /// offer (a separate query could race a share starting or stopping in between).
    async fn create_labelled_offer(&self) -> Result<PublishOffer, EngineError> {
        let sdp = self.create_publish_offer().await?;
        let tracks = default_labels(&sdp);
        Ok(PublishOffer { sdp, tracks })
    }
    /// Set the publish PC's remote description.
    async fn apply_publish_answer(&self, sdp: String) -> Result<(), EngineError>;
    /// Set the subscribe PC's remote description to `sdp`, create the answer and set it as
    /// the local description (a complete offer/answer transition); return the answer.
    /// `streams` maps this offer's mids to participants.
    async fn apply_subscribe_offer(
        &self,
        sdp: String,
        streams: Vec<SubStream>,
    ) -> Result<String, EngineError>;
    /// A remote candidate (`None`: end of candidates). MUST NOT block or do I/O.
    fn add_remote_candidate(
        &self,
        pc: PcKind,
        candidate: Option<IceCandidate>,
    ) -> Result<(), EngineError>;
    /// Local mic/camera on or off. MUST NOT block.
    fn set_local_media(&self, audio: bool, video: bool) -> Result<(), EngineError>;
    /// STUN/TURN servers for PeerConnections created from now on. MUST NOT block.
    fn set_ice_servers(&self, _servers: Vec<IceServer>) {}
    /// Stop capture and tear down both PCs. Called exactly once. Fences the engine: any
    /// operation still running must, when it completes, leave capture stopped and the PCs
    /// closed, and return an error.
    async fn close(&self);
}

/// Where the call is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallStatus {
    /// `call.join` sent, waiting for `call.joined`.
    Joining,
    /// In the call (media connectivity is the engine's to report).
    Connected,
    /// The socket dropped; resuming on the next one.
    Reconnecting,
    /// Over; see the reason.
    Ended(EndReason),
}

/// Why a call ended.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum EndReason {
    /// We left.
    Left,
    /// The SFU restarted.
    SfuRestart,
    /// We were removed from the channel.
    Removed,
    /// Another socket of the same user took this participant over.
    Replaced,
    /// The server no longer knows this participant (resume came too late).
    Expired,
    /// The session changed (signed out, or signed in as someone else).
    SessionChanged,
    /// The media engine failed.
    EngineFailed(String),
    /// The server refused an essential command (its error code).
    Server(String),
}

/// A snapshot of the call, published on a watch channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallState {
    /// Where the call is.
    pub status: CallStatus,
    /// The call, once joined.
    pub call_id: Option<String>,
    /// Our participant id, once joined.
    pub self_participant: Option<String>,
    /// Everyone else.
    pub participants: Vec<Participant>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn candidate_uses_the_exact_wire_keys() {
        let c = IceCandidate {
            candidate: "candidate:1 1 udp 1 10.0.0.1 5000 typ host".into(),
            sdp_mid: Some("0".into()),
            sdp_mline_index: Some(0),
        };
        let v = serde_json::to_value(&c).unwrap();
        assert_eq!(
            v,
            json!({ "candidate": c.candidate, "sdpMid": "0", "sdpMLineIndex": 0 })
        );
        assert_eq!(serde_json::from_value::<IceCandidate>(v).unwrap(), c);
    }

    #[test]
    fn pc_kind_is_lowercase_on_the_wire() {
        assert_eq!(
            serde_json::to_value(PcKind::Subscribe).unwrap(),
            json!("subscribe")
        );
    }

    #[test]
    fn unknown_kind_or_source_decodes_to_unknown() {
        let s: SubStream = serde_json::from_value(json!({
            "mid": "3", "participant_id": "p", "kind": "hologram", "source": "screen2"
        }))
        .unwrap();
        assert_eq!(s.kind, MediaKind::Unknown);
        assert_eq!(s.source, MediaSource::Unknown);
    }
}
