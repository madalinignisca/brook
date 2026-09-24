//! The call task: drives one call's signaling (PROTOCOL.md §3) and the platform
//! [`MediaEngine`]. See docs/superpowers/specs/2026-09-24-core-call-signaling-design.md §3.4.
//!
//! One task per call owns all call state. It never awaits an engine operation or a server
//! reply inline: those are spawned, and their results come back as [`Done`] messages
//! tagged with a per-PeerConnection sequence number, so the task always keeps servicing
//! signaling (and a result that belongs to superseded work is recognised and ignored).
//! Nothing is written to the socket except through the state rules below, and only on the
//! connection generation the call is currently joined on.

use std::collections::HashMap;
use std::sync::Arc;

use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot, watch};

use crate::call_types::{
    CallState, CallStatus, EndReason, EngineError, IceCandidate, IceServer, MediaEngine,
    Participant, PcKind, SubStream,
};
use crate::session_store::Revision;
use crate::ws::{CallFrame, CallRoute, CommandError, Commands, Conn, Reply, Routes};
use crate::{Error, Result};

type CmdResult = std::result::Result<Reply, CommandError>;

/// How long `leave()` waits for the server to confirm, so "await leave(); exit" does not
/// drop the socket before the server has removed us. Errors and timeouts are ignored.
const LEAVE_WAIT: std::time::Duration = std::time::Duration::from_secs(3);

/// A live call. Dropping it leaves the call.
pub struct CallHandle {
    input: mpsc::UnboundedSender<Input>,
    state: watch::Receiver<CallState>,
}

impl CallHandle {
    /// Observe the call (status, roster). Latest state wins.
    pub fn state(&self) -> watch::Receiver<CallState> {
        self.state.clone()
    }

    /// A local ICE candidate from the engine (`None`: gathering complete). Non-blocking;
    /// safe from any thread.
    pub fn local_candidate(&self, pc: PcKind, candidate: Option<IceCandidate>) {
        let _ = self.input.send(Input::LocalCandidate(pc, candidate));
    }

    /// The engine hit an unrecoverable error: end the call. Non-blocking.
    pub fn engine_failed(&self, message: String) {
        let _ = self.input.send(Input::EngineFailed(message));
    }

    /// Mute/unmute and camera on/off. The engine is updated first, then the server.
    pub async fn set_media(&self, audio: bool, video: bool) -> Result<()> {
        self.ask(|reply| Input::SetMedia {
            audio,
            video,
            reply,
        })
        .await
    }

    /// Renegotiate the publish PeerConnection (e.g. after adding a track).
    pub async fn republish(&self) -> Result<()> {
        self.ask(Input::Republish).await
    }

    /// Leave the call. Local media stops and the status becomes `Ended(Left)` immediately;
    /// this resolves once the server confirmed (or after a few seconds at most), so an app
    /// can await it and then quit.
    pub async fn leave(&self) -> Result<()> {
        self.ask(Input::Leave).await
    }

    async fn ask(&self, make: impl FnOnce(oneshot::Sender<Result<()>>) -> Input) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.input.send(make(tx)).map_err(|_| Error::CallEnded)?;
        rx.await.map_err(|_| Error::CallEnded)?
    }
}

impl Drop for CallHandle {
    fn drop(&mut self) {
        let _ = self.input.send(Input::HandleDropped);
    }
}

/// Join `channel_id`'s call on the current socket. Returns once `call.joined` arrived.
pub(crate) async fn join(
    commands: Commands,
    revisions: watch::Receiver<Revision>,
    channel_id: &str,
    engine: Arc<dyn MediaEngine>,
    publish: bool,
) -> Result<Arc<CallHandle>> {
    let conn = *commands.conn().borrow();
    if !conn.ready {
        return Err(Error::Disconnected);
    }
    let epoch = revisions.borrow().epoch;
    let (route_tx, route_rx) = mpsc::unbounded_channel();
    // The transport installs the route and hands it `call.joined` (first in the mailbox)
    // before it reads the next frame; the reply below is only the go-ahead.
    commands
        .request(
            conn.generation,
            json!({ "type": "call.join", "data": { "channel_id": channel_id } }),
            "call.joined",
            Some(route_tx.clone()),
        )
        .await?;

    let (input_tx, input_rx) = mpsc::unbounded_channel();
    let (state_tx, state_rx) = watch::channel(CallState {
        status: CallStatus::Joining,
        call_id: None,
        self_participant: None,
        participants: Vec::new(),
    });
    let task = Task {
        engine,
        conn_rx: commands.conn(),
        routes: commands.routes.clone(),
        commands,
        route_tx,
        state_tx,
        done_tx: input_tx.clone(),
        call_id: String::new(),
        participant_id: String::new(),
        resume_token: Secret::default(),
        generation: conn.generation,
        connected: true,
        resuming: false,
        epoch,
        publish_wanted: publish,
        publish: Publish::Idle,
        pub_seq: 0,
        unsent_offer: None,
        pub_candidates: Vec::new(),
        sub: Subscribe::default(),
        applied_mids: HashMap::new(),
        remote_buf: HashMap::new(),
        media: Media::default(),
        media_reply: None,
        ended: false,
    };
    tokio::spawn(task.run(route_rx, input_rx, revisions));
    Ok(Arc::new(CallHandle {
        input: input_tx,
        state: state_rx,
    }))
}

/// A resume token: kept only in the call task, never printed.
#[derive(Default, Clone)]
struct Secret(String);

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<redacted>")
    }
}

enum Input {
    LocalCandidate(PcKind, Option<IceCandidate>),
    EngineFailed(String),
    SetMedia {
        audio: bool,
        video: bool,
        reply: oneshot::Sender<Result<()>>,
    },
    Republish(oneshot::Sender<Result<()>>),
    Leave(oneshot::Sender<Result<()>>),
    HandleDropped,
    Done(Done),
}

/// Results of spawned work, tagged so superseded results are recognised.
enum Done {
    PublishOffer {
        seq: u64,
        res: std::result::Result<String, EngineError>,
    },
    PublishAnswer {
        seq: u64,
        res: CmdResult,
    },
    PublishApplied {
        seq: u64,
        sdp: String,
        res: std::result::Result<(), EngineError>,
    },
    SubscribeApplied {
        seq: u64,
        version: u64,
        streams: Vec<SubStream>,
        res: std::result::Result<String, EngineError>,
    },
    SubscribeAck {
        version: u64,
        res: CmdResult,
    },
    Resumed {
        generation: u64,
        res: CmdResult,
    },
    MediaAck {
        intent: (bool, bool),
        prev: (bool, bool),
        res: CmdResult,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Publish {
    Idle,
    /// `create_publish_offer` in flight.
    Offering(u64),
    /// `call.publish` written; waiting for the answer.
    AwaitingAnswer(u64),
    /// `apply_publish_answer` in flight.
    ApplyingAnswer(u64),
    Stable,
    /// The answer was lost with the socket; renegotiate after resume.
    NeedsRestart,
}

#[derive(Default)]
struct Subscribe {
    seq: u64,
    /// Newest offer not yet applied: (version, sdp, streams).
    latest: Option<(u64, String, Vec<SubStream>)>,
    /// (seq, version) of the offer being applied.
    applying: Option<(u64, u64)>,
    /// The one answer sent (or to send) and not yet acknowledged: (version, sdp).
    retained: Option<(u64, String)>,
    acked: u64,
}

#[derive(Default)]
struct Media {
    /// What the engine currently has.
    engine: Option<(bool, bool)>,
    in_flight: bool,
    queued: Option<(bool, bool, oneshot::Sender<Result<()>>)>,
    /// The server may not have the latest intent (e.g. dropped socket): re-send after resume.
    dirty: bool,
}

struct Task {
    engine: Arc<dyn MediaEngine>,
    commands: Commands,
    conn_rx: watch::Receiver<Conn>,
    routes: Routes,
    route_tx: CallRoute,
    state_tx: watch::Sender<CallState>,
    done_tx: mpsc::UnboundedSender<Input>,
    call_id: String,
    participant_id: String,
    resume_token: Secret,
    /// The connection generation this call is joined on.
    generation: u64,
    /// Joined (or resumed) on `generation`: only then may call commands be written.
    connected: bool,
    resuming: bool,
    epoch: u64,
    publish_wanted: bool,
    publish: Publish,
    pub_seq: u64,
    /// An offer that completed while the socket was down; sent after resume.
    unsent_offer: Option<(u64, String)>,
    /// Local publish candidates held until `call.publish` for the current offer is written.
    pub_candidates: Vec<Option<IceCandidate>>,
    sub: Subscribe,
    /// Mids of the currently applied description, per PC.
    applied_mids: HashMap<PcKind, Vec<String>>,
    /// Remote candidates waiting for a description that contains their mid.
    remote_buf: HashMap<PcKind, Vec<Option<IceCandidate>>>,
    media: Media,
    /// The caller waiting on the in-flight `call.media`.
    media_reply: Option<oneshot::Sender<Result<()>>>,
    ended: bool,
}

#[derive(Deserialize)]
struct Joined {
    call_id: String,
    #[serde(rename = "self")]
    me: SelfInfo,
    #[serde(default)]
    participants: Vec<Participant>,
    #[serde(default)]
    ice_servers: Vec<IceServer>,
}

#[derive(Deserialize)]
struct SelfInfo {
    participant_id: String,
    resume_token: String,
}

impl Task {
    async fn run(
        mut self,
        mut route_rx: mpsc::UnboundedReceiver<CallFrame>,
        mut input_rx: mpsc::UnboundedReceiver<Input>,
        mut revisions: watch::Receiver<Revision>,
    ) {
        let mut conn_rx = self.conn_rx.clone();
        while !self.ended {
            tokio::select! {
                Some(frame) = route_rx.recv() => self.on_frame(frame),
                Some(input) = input_rx.recv() => self.on_input(input),
                Ok(()) = conn_rx.changed() => {
                    let conn = *conn_rx.borrow_and_update();
                    self.on_conn(conn);
                }
                changed = revisions.changed() => {
                    if changed.is_err() || revisions.borrow_and_update().epoch != self.epoch {
                        self.finish(EndReason::SessionChanged, false);
                    }
                }
                else => break,
            }
        }
    }

    // ---- server frames ----

    fn on_frame(&mut self, frame: CallFrame) {
        let data = frame.data;
        match frame.ty.as_str() {
            "call.joined" => self.on_joined(data),
            "call.subscribe.offer" => {
                let version = data["version"].as_u64().unwrap_or(0);
                let sdp = data["sdp"].as_str().unwrap_or_default().to_string();
                let streams: Vec<SubStream> =
                    serde_json::from_value(data["streams"].clone()).unwrap_or_default();
                self.on_offer(version, sdp, streams);
            }
            "call.ice" => {
                let pc: Option<PcKind> = serde_json::from_value(data["pc"].clone()).ok();
                let candidate: Option<IceCandidate> =
                    serde_json::from_value(data["candidate"].clone()).ok();
                if let Some(pc) = pc {
                    self.on_remote_candidate(pc, candidate);
                }
            }
            "call.participant" => {
                let event = data["event"].as_str().unwrap_or_default().to_string();
                if let Ok(p) = serde_json::from_value::<Participant>(data["participant"].clone()) {
                    self.state_tx.send_modify(|s| {
                        s.participants
                            .retain(|x| x.participant_id != p.participant_id);
                        if event != "left" {
                            s.participants.push(p);
                        }
                    });
                }
            }
            "call.ended" => {
                let reason = match data["reason"].as_str().unwrap_or_default() {
                    "sfu_restart" => EndReason::SfuRestart,
                    "removed" => EndReason::Removed,
                    "replaced" => EndReason::Replaced,
                    other => EndReason::Server(other.to_string()),
                };
                self.finish(reason, false);
            }
            _ => {}
        }
    }

    fn on_joined(&mut self, data: Value) {
        let Ok(j) = serde_json::from_value::<Joined>(data) else {
            return self.finish(EndReason::Server("unexpected_response".into()), true);
        };
        let first = self.call_id.is_empty();
        self.call_id = j.call_id.clone();
        self.participant_id = j.me.participant_id.clone();
        // Always keep the newest token received (the server also accepts the previous one).
        self.resume_token = Secret(j.me.resume_token);
        if !j.ice_servers.is_empty() {
            self.engine.set_ice_servers(j.ice_servers);
        }
        self.state_tx.send_modify(|s| {
            s.call_id = Some(j.call_id);
            s.self_participant = Some(j.me.participant_id);
            s.participants = j.participants;
            if first {
                s.status = CallStatus::Connected;
            }
        });
        if first && self.publish_wanted {
            self.start_publish_offer();
        }
    }

    // ---- publish ----

    fn start_publish_offer(&mut self) {
        self.pub_seq += 1;
        let seq = self.pub_seq;
        self.publish = Publish::Offering(seq);
        self.pub_candidates.clear();
        let engine = self.engine.clone();
        let done = self.done_tx.clone();
        tokio::spawn(async move {
            let res = engine.create_publish_offer().await;
            let _ = done.send(Input::Done(Done::PublishOffer { seq, res }));
        });
    }

    fn send_publish(&mut self, seq: u64, sdp: String) {
        if !self.connected {
            self.unsent_offer = Some((seq, sdp));
            return;
        }
        let frame =
            json!({ "type": "call.publish", "data": { "call_id": self.call_id, "sdp": sdp } });
        match self
            .commands
            .start(self.generation, frame, "call.publish.answer", None)
        {
            Ok(rx) => {
                self.publish = Publish::AwaitingAnswer(seq);
                // Held candidates go out only now: the FIFO queue writes them after it.
                for c in std::mem::take(&mut self.pub_candidates) {
                    self.send_candidate(PcKind::Publish, c);
                }
                self.await_reply(rx, move |res| Done::PublishAnswer { seq, res });
            }
            Err(_) => {
                self.unsent_offer = Some((seq, sdp));
            }
        }
    }

    // ---- subscribe ----

    fn on_offer(&mut self, version: u64, sdp: String, streams: Vec<SubStream>) {
        if version <= self.sub.acked {
            return;
        }
        if self
            .sub
            .retained
            .as_ref()
            .is_some_and(|(v, _)| *v == version)
        {
            // The server's replay of an offer we already answered: resend, don't re-apply.
            self.send_retained_answer();
            return;
        }
        if self
            .sub
            .latest
            .as_ref()
            .is_some_and(|(v, _, _)| *v >= version)
            || self.sub.applying.is_some_and(|(_, v)| v >= version)
        {
            return;
        }
        self.sub.latest = Some((version, sdp, streams));
        self.maybe_apply_offer();
    }

    fn maybe_apply_offer(&mut self) {
        if self.sub.applying.is_some() {
            return;
        }
        let Some((version, sdp, streams)) = self.sub.latest.take() else {
            return;
        };
        self.sub.seq += 1;
        let seq = self.sub.seq;
        self.sub.applying = Some((seq, version));
        let engine = self.engine.clone();
        let done = self.done_tx.clone();
        tokio::spawn(async move {
            let res = engine.apply_subscribe_offer(sdp, streams.clone()).await;
            let _ = done.send(Input::Done(Done::SubscribeApplied {
                seq,
                version,
                streams,
                res,
            }));
        });
    }

    fn send_retained_answer(&mut self) {
        let Some((version, sdp)) = self.sub.retained.clone() else {
            return;
        };
        if !self.connected {
            return; // resent after resume
        }
        let frame = json!({ "type": "call.subscribe.answer",
            "data": { "call_id": self.call_id, "version": version, "sdp": sdp } });
        if let Ok(rx) = self.commands.start(self.generation, frame, "call.ok", None) {
            self.await_reply(rx, move |res| Done::SubscribeAck { version, res });
        }
    }

    // ---- ICE ----

    fn on_remote_candidate(&mut self, pc: PcKind, candidate: Option<IceCandidate>) {
        let buffered = self.remote_buf.get(&pc).is_some_and(|b| !b.is_empty());
        if buffered || !self.applicable(pc, &candidate) {
            self.remote_buf.entry(pc).or_default().push(candidate);
            return;
        }
        self.add_remote(pc, candidate);
    }

    /// Whether `candidate` belongs to a media section of `pc`'s applied description.
    fn applicable(&self, pc: PcKind, candidate: &Option<IceCandidate>) -> bool {
        let Some(mids) = self.applied_mids.get(&pc) else {
            return false; // no description applied yet
        };
        match candidate {
            None => true, // end-of-candidates: only reached when nothing is buffered before it
            Some(c) => match (&c.sdp_mid, c.sdp_mline_index) {
                (Some(mid), _) => mids.contains(mid),
                (None, Some(index)) => (index as usize) < mids.len(),
                (None, None) => false,
            },
        }
    }

    /// A description was applied: flush buffered candidates in order, stopping at the first
    /// one that still has no matching media section (order is preserved).
    fn flush_remote(&mut self, pc: PcKind) {
        let buffered = self.remote_buf.remove(&pc).unwrap_or_default();
        let mut rest = Vec::new();
        for c in buffered {
            if rest.is_empty() && self.applicable(pc, &c) {
                self.add_remote(pc, c);
                if self.ended {
                    return;
                }
            } else {
                rest.push(c);
            }
        }
        if !rest.is_empty() {
            self.remote_buf.insert(pc, rest);
        }
    }

    fn add_remote(&mut self, pc: PcKind, candidate: Option<IceCandidate>) {
        if let Err(err) = self.engine.add_remote_candidate(pc, candidate) {
            self.finish(EndReason::EngineFailed(err.0), true);
        }
    }

    fn on_local_candidate(&mut self, pc: PcKind, candidate: Option<IceCandidate>) {
        if pc == PcKind::Publish && matches!(self.publish, Publish::Offering(_)) {
            self.pub_candidates.push(candidate); // until `call.publish` is written
            return;
        }
        self.send_candidate(pc, candidate);
    }

    fn send_candidate(&mut self, pc: PcKind, candidate: Option<IceCandidate>) {
        if !self.connected {
            return; // no ICE restart in v1: the PC's ICE session survives; nothing to replay
        }
        let frame = json!({ "type": "call.ice",
            "data": { "call_id": self.call_id, "pc": pc, "candidate": candidate } });
        let _ = self.commands.notify(self.generation, frame);
    }

    // ---- inputs ----

    fn on_input(&mut self, input: Input) {
        match input {
            Input::LocalCandidate(pc, c) => self.on_local_candidate(pc, c),
            Input::EngineFailed(msg) => self.finish(EndReason::EngineFailed(msg), true),
            Input::SetMedia {
                audio,
                video,
                reply,
            } => self.set_media(audio, video, reply),
            Input::Republish(reply) => {
                if self.publish == Publish::Stable && self.connected {
                    self.start_publish_offer();
                    let _ = reply.send(Ok(()));
                } else {
                    let _ = reply.send(Err(Error::Busy));
                }
            }
            Input::Leave(reply) => {
                // Start `call.leave` with its reply kept (the transport skips requests whose
                // receiver is gone), then tear down locally without waiting for it.
                let confirm = if self.connected && !self.ended {
                    let frame =
                        json!({ "type": "call.leave", "data": { "call_id": self.call_id } });
                    self.commands
                        .start(self.generation, frame, "call.ok", None)
                        .ok()
                } else {
                    None
                };
                self.finish(EndReason::Left, confirm.is_none());
                tokio::spawn(async move {
                    if let Some(rx) = confirm {
                        let _ = tokio::time::timeout(LEAVE_WAIT, rx).await;
                    }
                    let _ = reply.send(Ok(()));
                });
            }
            Input::HandleDropped => self.finish(EndReason::Left, true),
            Input::Done(done) => self.on_done(done),
        }
    }

    fn set_media(&mut self, audio: bool, video: bool, reply: oneshot::Sender<Result<()>>) {
        if self.media.in_flight {
            // One `call.media` at a time; a newer intent replaces a queued one.
            if let Some((_, _, older)) = self.media.queued.replace((audio, video, reply)) {
                let _ = older.send(Ok(()));
            }
            return;
        }
        let prev = self.media.engine.unwrap_or((true, true));
        if let Err(err) = self.engine.set_local_media(audio, video) {
            let _ = reply.send(Err(Error::Api {
                code: "engine".into(),
                message: err.0,
            }));
            return;
        }
        self.media.engine = Some((audio, video));
        self.send_media(audio, video, prev, Some(reply));
    }

    /// Tell the server the media state the engine already has.
    fn send_media(
        &mut self,
        audio: bool,
        video: bool,
        prev: (bool, bool),
        reply: Option<oneshot::Sender<Result<()>>>,
    ) {
        let frame = json!({ "type": "call.media",
            "data": { "call_id": self.call_id, "audio": audio, "video": video } });
        let started = if self.connected {
            self.commands
                .start(self.generation, frame, "call.ok", None)
                .ok()
        } else {
            None
        };
        match started {
            Some(rx) => {
                self.media.in_flight = true;
                self.media_reply = reply;
                let intent = (audio, video);
                self.await_reply(rx, move |res| Done::MediaAck { intent, prev, res });
            }
            None => {
                // Not connected: the engine has it; the server gets it after resume.
                self.media.dirty = true;
                if let Some(r) = reply {
                    let _ = r.send(Ok(()));
                }
            }
        }
    }

    fn on_done(&mut self, done: Done) {
        match done {
            Done::PublishOffer { seq, res } => {
                if seq != self.pub_seq || self.publish != Publish::Offering(seq) {
                    return;
                }
                match res {
                    Ok(sdp) => self.send_publish(seq, sdp),
                    Err(err) => self.finish(EndReason::EngineFailed(err.0), true),
                }
            }
            Done::PublishAnswer { seq, res } => {
                if self.publish != Publish::AwaitingAnswer(seq) {
                    return;
                }
                match res {
                    Ok(reply) => {
                        let sdp = reply.data["sdp"].as_str().unwrap_or_default().to_string();
                        self.publish = Publish::ApplyingAnswer(seq);
                        let (engine, done) = (self.engine.clone(), self.done_tx.clone());
                        tokio::spawn(async move {
                            let res = engine.apply_publish_answer(sdp.clone()).await;
                            let _ = done.send(Input::Done(Done::PublishApplied { seq, sdp, res }));
                        });
                    }
                    // The socket went away with the answer: renegotiate after resume.
                    Err(CommandError::Unknown) | Err(CommandError::NotSent) => {
                        self.publish = Publish::NeedsRestart;
                    }
                    Err(err) => self.finish(EndReason::Server(code_of(&err)), true),
                }
            }
            Done::PublishApplied { seq, sdp, res } => {
                if self.publish != Publish::ApplyingAnswer(seq) {
                    return;
                }
                match res {
                    Ok(()) => {
                        self.applied_mids.insert(PcKind::Publish, mids_of(&sdp));
                        self.publish = Publish::Stable;
                        self.flush_remote(PcKind::Publish);
                    }
                    Err(err) => self.finish(EndReason::EngineFailed(err.0), true),
                }
            }
            Done::SubscribeApplied {
                seq,
                version,
                streams,
                res,
            } => {
                if self.sub.applying.map(|(s, _)| s) != Some(seq) {
                    return;
                }
                self.sub.applying = None;
                match res {
                    Ok(answer) => {
                        self.applied_mids.insert(
                            PcKind::Subscribe,
                            streams.iter().map(|s| s.mid.clone()).collect(),
                        );
                        self.flush_remote(PcKind::Subscribe);
                        if self.ended {
                            return;
                        }
                        if self.sub.latest.is_some() {
                            // Superseded while applying: skip this answer, apply the newer offer.
                            self.maybe_apply_offer();
                        } else {
                            self.sub.retained = Some((version, answer));
                            self.send_retained_answer();
                        }
                    }
                    Err(err) => self.finish(EndReason::EngineFailed(err.0), true),
                }
            }
            Done::SubscribeAck { version, res } => {
                let holds = self
                    .sub
                    .retained
                    .as_ref()
                    .is_some_and(|(v, _)| *v == version);
                match res {
                    Ok(_) => {
                        self.sub.acked = self.sub.acked.max(version);
                        if holds {
                            self.sub.retained = None;
                        }
                    }
                    Err(CommandError::Rejected { code, .. }) if code == "stale" => {
                        if holds {
                            self.sub.retained = None;
                        }
                    }
                    // Outcome unknown: keep the answer; the server replays the offer after resume.
                    Err(CommandError::Unknown)
                    | Err(CommandError::NotSent)
                    | Err(CommandError::Timeout) => {}
                    Err(err) => self.finish(EndReason::Server(code_of(&err)), true),
                }
            }
            Done::Resumed { generation, res } => {
                self.resuming = false;
                match res {
                    Ok(_) => {
                        self.generation = generation;
                        self.connected = true;
                        self.state_tx
                            .send_modify(|s| s.status = CallStatus::Connected);
                        self.after_resume();
                    }
                    Err(CommandError::Rejected { code, .. }) if code == "not_in_call" => {
                        self.finish(EndReason::Expired, false);
                    }
                    Err(CommandError::Rejected { code, .. }) => {
                        self.finish(EndReason::Server(code), false)
                    }
                    // Socket dropped again (or no answer): try again on the next socket.
                    Err(_) => {
                        let conn = *self.conn_rx.borrow();
                        self.on_conn(conn);
                    }
                }
            }
            Done::MediaAck { intent, prev, res } => {
                self.media.in_flight = false;
                let reply = self.media_reply.take();
                match res {
                    Ok(_) => {
                        if let Some(r) = reply {
                            let _ = r.send(Ok(()));
                        }
                    }
                    Err(CommandError::Rejected { code, message }) => {
                        // Rejected: roll back — unless a newer intent is already applied.
                        if self.media.engine == Some(intent)
                            && self.media.queued.is_none()
                            && self.engine.set_local_media(prev.0, prev.1).is_ok()
                        {
                            self.media.engine = Some(prev);
                        }
                        if let Some(r) = reply {
                            let _ = r.send(Err(Error::Api { code, message }));
                        }
                    }
                    Err(err) => {
                        // Outcome unknown: the server may have applied it. Keep the engine as
                        // is and re-send the latest intent after resume.
                        self.media.dirty = true;
                        if let Some(r) = reply {
                            let _ = r.send(Err(err.into()));
                        }
                    }
                }
                if let Some((a, v, r)) = self.media.queued.take() {
                    self.set_media(a, v, r);
                }
            }
        }
    }

    // ---- connection changes and resume ----

    fn on_conn(&mut self, conn: Conn) {
        if self.ended {
            return;
        }
        if (!conn.ready || conn.generation != self.generation) && self.connected {
            self.connected = false;
            self.state_tx
                .send_modify(|s| s.status = CallStatus::Reconnecting);
        }
        if conn.ready && conn.generation > self.generation && !self.resuming {
            let frame = json!({ "type": "call.resume", "data": {
                "call_id": self.call_id,
                "participant_id": self.participant_id,
                "resume_token": self.resume_token.0,
            }});
            let generation = conn.generation;
            if let Ok(rx) = self.commands.start(
                generation,
                frame,
                "call.joined",
                Some(self.route_tx.clone()),
            ) {
                self.resuming = true;
                self.await_reply(rx, move |res| Done::Resumed { generation, res });
            }
        }
    }

    /// Redo the work the drop interrupted, now that the call is resumed.
    fn after_resume(&mut self) {
        match self.publish {
            Publish::NeedsRestart | Publish::AwaitingAnswer(_) => self.start_publish_offer(),
            _ => {}
        }
        if let Some((seq, sdp)) = self.unsent_offer.take() {
            if self.publish == Publish::Offering(seq) {
                self.send_publish(seq, sdp);
            }
        }
        self.send_retained_answer();
        if self.media.dirty && !self.media.in_flight {
            self.media.dirty = false;
            if let Some((a, v)) = self.media.engine {
                self.send_media(a, v, (a, v), None);
            }
        }
    }

    // ---- helpers ----

    fn await_reply(
        &self,
        rx: oneshot::Receiver<CmdResult>,
        wrap: impl FnOnce(CmdResult) -> Done + Send + 'static,
    ) {
        let done = self.done_tx.clone();
        tokio::spawn(async move {
            let res = rx.await.unwrap_or(Err(CommandError::Unknown));
            let _ = done.send(Input::Done(wrap(res)));
        });
    }

    /// The single end path: first reason wins; the engine is closed immediately and exactly
    /// once; `leave` tells the server best-effort without delaying the teardown.
    fn finish(&mut self, reason: EndReason, tell_server: bool) {
        if self.ended {
            return;
        }
        self.ended = true;
        self.state_tx
            .send_modify(|s| s.status = CallStatus::Ended(reason));
        if !self.call_id.is_empty() {
            self.routes.lock().unwrap().remove(&self.call_id);
        }
        // Exactly once: `ended` above guards every path here.
        let engine = self.engine.clone();
        tokio::spawn(async move { engine.close().await });
        if tell_server && self.connected {
            // Fire-and-forget: nothing waits for the reply. (A request whose reply receiver
            // is dropped is treated by the transport as abandoned and never written.)
            let frame = json!({ "type": "call.leave", "data": { "call_id": self.call_id } });
            let _ = self.commands.notify(self.generation, frame);
        }
        if let Some(r) = self.media_reply.take() {
            let _ = r.send(Err(Error::CallEnded));
        }
        if let Some((_, _, r)) = self.media.queued.take() {
            let _ = r.send(Err(Error::CallEnded));
        }
    }
}

fn code_of(err: &CommandError) -> String {
    match err {
        CommandError::Rejected { code, .. } => code.clone(),
        CommandError::Timeout => "timeout".into(),
        CommandError::UnexpectedReply => "unexpected_response".into(),
        CommandError::TooLarge => "too_large".into(),
        CommandError::Busy => "busy".into(),
        CommandError::NotSent | CommandError::Unknown => "disconnected".into(),
    }
}

/// The mids of an SDP's media sections, in order.
fn mids_of(sdp: &str) -> Vec<String> {
    sdp.lines()
        .filter_map(|l| l.trim().strip_prefix("a=mid:"))
        .map(str::to_string)
        .collect()
}
