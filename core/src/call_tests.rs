//! Call task tests: the real client and transport against the scripted test origin, with
//! a fake engine whose operations can be held at gates.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::sync::Semaphore;

use crate::test_support::{TestServer, WsPeer};
use crate::{
    default_labels, BrookClient, CallHandle, CallStatus, EndReason, EngineError, IceCandidate,
    MediaEngine, MediaKind, MediaSource, PcKind, PublishOffer, SubStream,
};

const WAIT: Duration = Duration::from_secs(5);
const QUIET: Duration = Duration::from_millis(300);

#[derive(Default)]
struct FakeEngine {
    log: Mutex<Vec<String>>,
    gates: Mutex<HashMap<&'static str, Arc<Semaphore>>>,
    closes: AtomicUsize,
    closed: AtomicBool,
    capture: AtomicBool,
    offers: AtomicUsize,
    subscribe_applies: AtomicUsize,
    fail_media: AtomicBool,
    fail_candidates: AtomicBool,
    /// When set, offers are this SDP instead of "offer-N".
    offer_sdp: Mutex<Option<String>>,
}

impl FakeEngine {
    fn gate(&self, op: &'static str) -> Arc<Semaphore> {
        let sem = Arc::new(Semaphore::new(0));
        self.gates.lock().unwrap().insert(op, sem.clone());
        sem
    }
    async fn pass(&self, op: &'static str) {
        let gate = self.gates.lock().unwrap().get(op).cloned();
        if let Some(gate) = gate {
            gate.acquire().await.unwrap().forget();
        }
    }
    fn record(&self, entry: impl Into<String>) {
        self.log.lock().unwrap().push(entry.into());
    }
    fn log(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }
}

#[async_trait]
impl MediaEngine for FakeEngine {
    async fn create_publish_offer(&self) -> Result<String, EngineError> {
        self.record("create_publish_offer");
        self.capture.store(true, Ordering::SeqCst); // capture starts…
        self.pass("create_publish_offer").await;
        if self.closed.load(Ordering::SeqCst) {
            self.capture.store(false, Ordering::SeqCst); // …fenced by close
            return Err(EngineError("closed".into()));
        }
        let n = self.offers.fetch_add(1, Ordering::SeqCst) + 1;
        if let Some(sdp) = self.offer_sdp.lock().unwrap().clone() {
            return Ok(sdp);
        }
        Ok(format!("offer-{n}"))
    }
    async fn apply_publish_answer(&self, sdp: String) -> Result<(), EngineError> {
        self.record(format!(
            "apply_publish_answer:{}",
            sdp.lines().next().unwrap_or("")
        ));
        self.pass("apply_publish_answer").await;
        Ok(())
    }
    async fn apply_subscribe_offer(
        &self,
        sdp: String,
        _streams: Vec<SubStream>,
    ) -> Result<String, EngineError> {
        self.subscribe_applies.fetch_add(1, Ordering::SeqCst);
        self.record(format!("apply_subscribe_offer:{sdp}"));
        self.pass("apply_subscribe_offer").await;
        Ok(format!("answer-to-{sdp}"))
    }
    fn add_remote_candidate(&self, pc: PcKind, c: Option<IceCandidate>) -> Result<(), EngineError> {
        if self.fail_candidates.load(Ordering::SeqCst) {
            return Err(EngineError("ice failed".into()));
        }
        let what = c.map(|c| c.candidate).unwrap_or_else(|| "end".into());
        self.record(format!("remote:{pc:?}:{what}"));
        Ok(())
    }
    fn set_local_media(&self, audio: bool, video: bool) -> Result<(), EngineError> {
        if self.fail_media.load(Ordering::SeqCst) {
            return Err(EngineError("no device".into()));
        }
        self.record(format!("media:{audio}:{video}"));
        Ok(())
    }
    async fn close(&self) {
        let n = self.closes.fetch_add(1, Ordering::SeqCst) + 1;
        assert_eq!(n, 1, "engine closed twice");
        self.closed.store(true, Ordering::SeqCst);
        self.capture.store(false, Ordering::SeqCst);
        self.record("close");
    }
}

fn stream(mid: &str) -> Value {
    json!({ "mid": mid, "participant_id": "p2", "kind": "video", "source": "camera" })
}

fn cand(text: &str, mid: &str) -> Value {
    json!({ "candidate": text, "sdpMid": mid, "sdpMLineIndex": 0 })
}

struct Call {
    client: Arc<BrookClient>,
    peer: WsPeer,
    handle: Arc<CallHandle>,
    engine: Arc<FakeEngine>,
    server: TestServer,
}

async fn connected(server: &mut TestServer) -> (Arc<BrookClient>, WsPeer) {
    connected_with(server, |_| {}).await
}

async fn connected_with(
    server: &mut TestServer,
    tweak: impl FnOnce(&BrookClient),
) -> (Arc<BrookClient>, WsPeer) {
    let client = Arc::new(server.client());
    tweak(&client);
    client.login("alice", "pw").await.unwrap();
    client.start_realtime().await.unwrap();
    let mut peer = server.accept().await;
    peer.accept_auth().await;
    client.commands.conn().wait_for(|c| c.ready).await.unwrap();
    (client, peer)
}

fn joined(re: &Value, token: &str) -> Value {
    json!({ "type": "call.joined", "re": re, "data": {
        "call_id": "k1", "channel_id": "ch",
        "self": { "participant_id": "me", "resume_token": token },
        "participants": [{ "participant_id": "p2", "user_id": "u2", "display_name": "Bob",
                           "audio": true, "video": true, "publishing": [] }]
    }})
}

/// A joined call. `publish`: whether we publish. `setup` configures the engine first.
async fn join(publish: bool, setup: impl FnOnce(&FakeEngine)) -> Call {
    join_with(publish, setup, |_| {}).await
}

async fn join_with(
    publish: bool,
    setup: impl FnOnce(&FakeEngine),
    tweak: impl FnOnce(&BrookClient),
) -> Call {
    let mut server = TestServer::start().await;
    let (client, mut peer) = connected_with(&mut server, tweak).await;
    let engine = Arc::new(FakeEngine::default());
    setup(&engine);
    let (c, e) = (client.clone(), engine.clone());
    let joining = tokio::spawn(async move { c.join_call("ch", e, publish).await });
    let f = peer.recv().await;
    assert_eq!(f["type"], "call.join");
    peer.send(joined(&f["id"], "t1")).await;
    let handle = tokio::time::timeout(WAIT, joining)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    Call {
        client,
        peer,
        handle,
        engine,
        server,
    }
}

impl Call {
    async fn recv_type(&mut self, ty: &str) -> Value {
        loop {
            let f = self.peer.recv().await;
            if f["type"] == ty {
                return f;
            }
        }
    }
    async fn ok(&mut self, f: &Value) {
        self.peer
            .send(json!({ "type": "call.ok", "re": f["id"], "data": {} }))
            .await;
    }
    async fn offer(&mut self, version: u64, streams: Vec<Value>) {
        self.peer
            .send(json!({ "type": "call.subscribe.offer", "data": {
                "call_id": "k1", "version": version, "sdp": format!("sub-v{version}"), "streams": streams }}))
            .await;
    }
    /// Drop the socket and let the client come back; returns the `call.resume` frame.
    async fn reconnect(&mut self) -> Value {
        self.peer.close(1000, "drop").await;
        self.peer = self.server.accept().await;
        self.peer.accept_auth().await;
        let resume = self.peer.recv().await;
        assert_eq!(
            resume["type"], "call.resume",
            "first call frame on a new socket must be call.resume"
        );
        resume
    }
    async fn answer_publish(&mut self, publish: &Value) {
        self.peer
            .send(json!({ "type": "call.publish.answer", "re": publish["id"],
                "data": { "call_id": "k1", "sdp": "pub-answer\na=mid:0" } }))
            .await;
        self.eventually("answer applied", |l| {
            l.iter().any(|e| e.starts_with("apply_publish_answer"))
        })
        .await;
    }
    /// Frames are written in order, so if the next frame after a leave is `call.leave`, the
    /// client sent nothing else in between.
    async fn assert_nothing_before_leave(&mut self) {
        let h = self.handle.clone();
        tokio::spawn(async move { h.leave().await });
        let next = self.peer.recv().await;
        assert_eq!(next["type"], "call.leave", "unexpected frame {next}");
    }
    async fn wait_status(&self, want: impl Fn(&CallStatus) -> bool) {
        let mut st = self.handle.state();
        tokio::time::timeout(WAIT, st.wait_for(|s| want(&s.status)))
            .await
            .expect("status never reached")
            .unwrap();
    }
    async fn eventually(&self, what: &str, f: impl Fn(&[String]) -> bool) {
        for _ in 0..500 {
            if f(&self.engine.log()) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("never: {what}; log = {:?}", self.engine.log());
    }
}

// ---- join and publish ----

#[tokio::test]
async fn join_then_publish_offer_answer() {
    let mut call = join(true, |_| {}).await;
    call.wait_status(|s| *s == CallStatus::Connected).await;
    assert_eq!(
        call.handle.state().borrow().participants[0].display_name,
        "Bob"
    );
    let publish = call.recv_type("call.publish").await;
    assert_eq!(
        publish["data"],
        json!({ "call_id": "k1", "sdp": "offer-1", "tracks": [] })
    );
    call.peer
        .send(json!({ "type": "call.publish.answer", "re": publish["id"],
            "data": { "call_id": "k1", "sdp": "pub-answer\na=mid:0\na=mid:1" } }))
        .await;
    call.eventually("answer applied", |l| {
        l.iter().any(|e| e == "apply_publish_answer:pub-answer")
    })
    .await;
}

/// Publish candidates the engine emits before `call.publish` is written go out after it.
#[tokio::test]
async fn publish_candidates_wait_for_call_publish() {
    let mut call = join(true, |e| {
        e.gate("create_publish_offer");
    })
    .await;
    call.eventually("offer started", |l| {
        l.iter().any(|e| e == "create_publish_offer")
    })
    .await;
    let c = IceCandidate {
        candidate: "c-early".into(),
        sdp_mid: Some("0".into()),
        sdp_mline_index: Some(0),
    };
    call.handle.local_candidate(PcKind::Publish, Some(c));
    tokio::time::sleep(QUIET).await;
    call.engine.gates.lock().unwrap()["create_publish_offer"].add_permits(1);
    assert_eq!(call.peer.recv().await["type"], "call.publish");
    let ice = call.peer.recv().await;
    assert_eq!(ice["type"], "call.ice");
    assert_eq!(ice["data"]["candidate"]["candidate"], "c-early");
}

// ---- subscribe ----

/// `call.joined` then the first offer back-to-back: the offer is applied and answered.
#[tokio::test]
async fn first_offer_right_behind_joined_is_answered() {
    let mut server = TestServer::start().await;
    let (client, mut peer) = connected(&mut server).await;
    let engine = Arc::new(FakeEngine::default());
    let (c, e) = (client.clone(), engine.clone());
    let joining = tokio::spawn(async move { c.join_call("ch", e, false).await });
    let f = peer.recv().await;
    peer.send(joined(&f["id"], "t1")).await;
    peer.send(json!({ "type": "call.subscribe.offer", "data": {
        "call_id": "k1", "version": 1, "sdp": "sub-v1", "streams": [stream("0")] }}))
        .await;
    let _handle = joining.await.unwrap().unwrap();
    let answer = peer.recv().await;
    assert_eq!(answer["type"], "call.subscribe.answer");
    assert_eq!(answer["data"]["version"], 1);
    assert_eq!(answer["data"]["sdp"], "answer-to-sub-v1");
}

/// v2 and v3 arrive while v1 is being applied, and a local candidate is emitted meanwhile:
/// the candidate still goes out, only v3 is answered (v2 is never applied).
#[tokio::test]
async fn superseded_offers_are_skipped_and_signaling_keeps_flowing() {
    let mut call = join(false, |e| {
        e.gate("apply_subscribe_offer");
    })
    .await;
    call.offer(1, vec![stream("0")]).await;
    call.eventually("v1 applying", |l| {
        l.iter().any(|e| e == "apply_subscribe_offer:sub-v1")
    })
    .await;
    call.offer(2, vec![stream("0")]).await;
    call.offer(3, vec![stream("0"), stream("1")]).await;
    // Ordering signal: the call mailbox is FIFO, so once this roster event is visible in the
    // state, v2 and v3 have been processed by the call task too.
    call.peer
        .send(
            json!({ "type": "call.participant", "data": { "call_id": "k1", "event": "joined",
            "participant": { "participant_id": "p3", "user_id": "u3", "display_name": "Cy",
                             "audio": true, "video": true, "publishing": [] } } }),
        )
        .await;
    let mut st = call.handle.state();
    tokio::time::timeout(
        WAIT,
        st.wait_for(|s| s.participants.iter().any(|p| p.participant_id == "p3")),
    )
    .await
    .unwrap()
    .unwrap();
    let c = IceCandidate {
        candidate: "c-sub".into(),
        sdp_mid: Some("0".into()),
        sdp_mline_index: Some(0),
    };
    call.handle.local_candidate(PcKind::Subscribe, Some(c));
    let ice = call.peer.recv().await;
    assert_eq!(
        ice["type"], "call.ice",
        "signaling stalled behind the engine"
    );
    call.engine.gates.lock().unwrap()["apply_subscribe_offer"].add_permits(10);
    let answer = call.recv_type("call.subscribe.answer").await;
    assert_eq!(answer["data"]["version"], 3);
    assert!(!call
        .engine
        .log()
        .iter()
        .any(|e| e == "apply_subscribe_offer:sub-v2"));
}

/// After resume the server replays the unanswered offer (same version): the retained
/// answer is resent, the engine is not asked again.
#[tokio::test]
async fn replayed_offer_gets_the_retained_answer() {
    let mut call = join(false, |_| {}).await;
    call.offer(1, vec![stream("0")]).await;
    call.recv_type("call.subscribe.answer").await; // never acknowledged
    let resume = call.reconnect().await;
    call.peer.send(joined(&resume["id"], "t2")).await;
    call.wait_status(|s| *s == CallStatus::Connected).await;
    // The client resends the retained answer on its own after resume, and again for the
    // server's replay; neither re-applies.
    call.offer(1, vec![stream("0")]).await;
    let again = call.recv_type("call.subscribe.answer").await;
    assert_eq!(again["data"]["version"], 1);
    tokio::time::sleep(QUIET).await;
    assert_eq!(call.engine.subscribe_applies.load(Ordering::SeqCst), 1);
}

/// A late `stale` for v1 must not clear the retained v2 answer.
#[tokio::test]
async fn late_stale_for_an_old_version_keeps_the_newer_answer() {
    let mut call = join(false, |_| {}).await;
    call.offer(1, vec![stream("0")]).await;
    let a1 = call.recv_type("call.subscribe.answer").await;
    call.offer(2, vec![stream("0")]).await;
    let a2 = call.recv_type("call.subscribe.answer").await;
    assert_eq!(a2["data"]["version"], 2);
    call.peer
        .send(
            json!({ "type": "error", "re": a1["id"], "data": { "code": "stale", "message": "" } }),
        )
        .await;
    tokio::time::sleep(QUIET).await;
    let resume = call.reconnect().await; // v2 never acknowledged
    call.peer.send(joined(&resume["id"], "t2")).await;
    call.offer(2, vec![stream("0")]).await;
    let again = call.recv_type("call.subscribe.answer").await;
    assert_eq!(again["data"]["version"], 2);
    tokio::time::sleep(QUIET).await;
    assert_eq!(
        call.engine.subscribe_applies.load(Ordering::SeqCst),
        2,
        "v2 was re-applied"
    );
}

/// A late `call.ok` for v1 after v2 was acknowledged must not lower `acked`: a replay of
/// v2 is then ignored, not re-applied.
#[tokio::test]
async fn acknowledgements_never_move_backwards() {
    let mut call = join(false, |_| {}).await;
    call.offer(1, vec![stream("0")]).await;
    let a1 = call.recv_type("call.subscribe.answer").await;
    call.offer(2, vec![stream("0")]).await;
    let a2 = call.recv_type("call.subscribe.answer").await;
    call.ok(&a2).await;
    call.ok(&a1).await; // late
    tokio::time::sleep(QUIET).await;
    call.offer(2, vec![stream("0")]).await;
    tokio::time::sleep(QUIET).await;
    assert_eq!(call.engine.subscribe_applies.load(Ordering::SeqCst), 2);
}

// ---- ICE ----

/// A remote candidate for a mid that is not yet in the applied description waits for the
/// re-offer that adds it, and stays in order with end-of-candidates.
#[tokio::test]
async fn remote_candidate_for_a_new_mid_waits_for_its_description() {
    let mut call = join(false, |_| {}).await;
    call.offer(1, vec![stream("0")]).await;
    let a1 = call.recv_type("call.subscribe.answer").await;
    call.ok(&a1).await;
    call.peer.send(json!({ "type": "call.ice", "data": { "call_id": "k1", "pc": "subscribe", "candidate": cand("c-mid1", "1") } })).await;
    call.peer.send(json!({ "type": "call.ice", "data": { "call_id": "k1", "pc": "subscribe", "candidate": null } })).await;
    tokio::time::sleep(QUIET).await;
    assert!(
        !call.engine.log().iter().any(|e| e.starts_with("remote:")),
        "applied before its mid existed"
    );
    call.offer(2, vec![stream("0"), stream("1")]).await;
    call.eventually("flushed in order", |l| {
        let remote: Vec<&String> = l.iter().filter(|e| e.starts_with("remote:")).collect();
        remote.len() == 2
            && remote[0] == "remote:Subscribe:c-mid1"
            && remote[1] == "remote:Subscribe:end"
    })
    .await;
}

// ---- resume ----

/// The answer to `call.publish` is lost with the socket: after resume a new offer is sent.
#[tokio::test]
async fn publish_interrupted_by_a_drop_is_renegotiated_after_resume() {
    let mut call = join(true, |_| {}).await;
    call.recv_type("call.publish").await; // answer never comes
    let resume = call.reconnect().await;
    call.peer.send(joined(&resume["id"], "t2")).await;
    let publish = call.recv_type("call.publish").await;
    assert_eq!(publish["data"]["sdp"], "offer-2");
}

/// On a new socket, nothing but `call.resume` is written before its `call.joined`.
#[tokio::test]
async fn only_resume_before_resumed() {
    let mut call = join(false, |_| {}).await;
    call.peer.close(1000, "drop").await;
    call.wait_status(|s| *s == CallStatus::Reconnecting).await;
    let h = call.handle.clone();
    tokio::spawn(async move { h.set_media(false, true).await });
    call.handle.local_candidate(PcKind::Subscribe, None);
    call.peer = call.server.accept().await;
    call.peer.accept_auth().await;
    let resume = call.peer.recv().await;
    assert_eq!(resume["type"], "call.resume");
    assert!(
        tokio::time::timeout(QUIET, call.peer.recv()).await.is_err(),
        "wrote a call command before being resumed"
    );
    call.peer.send(joined(&resume["id"], "t2")).await;
    let media = call.recv_type("call.media").await; // the intent made while down
    assert_eq!(media["data"]["audio"], false);
}

/// Each resume uses the newest token received.
#[tokio::test]
async fn resume_uses_the_rotated_token() {
    let mut call = join(false, |_| {}).await;
    let r1 = call.reconnect().await;
    assert_eq!(r1["data"]["resume_token"], "t1");
    assert_eq!(r1["data"]["participant_id"], "me");
    call.peer.send(joined(&r1["id"], "t2")).await;
    call.wait_status(|s| *s == CallStatus::Connected).await;
    let r2 = call.reconnect().await;
    assert_eq!(r2["data"]["resume_token"], "t2");
}

#[tokio::test]
async fn resume_refused_ends_the_call_expired() {
    let mut call = join(false, |_| {}).await;
    let r = call.reconnect().await;
    call.peer.send(json!({ "type": "error", "re": r["id"], "data": { "code": "not_in_call", "message": "" } })).await;
    call.wait_status(|s| *s == CallStatus::Ended(EndReason::Expired))
        .await;
    call.eventually("engine closed", |l| l.iter().any(|e| e == "close"))
        .await;
}

// ---- mute ----

#[tokio::test]
async fn rejected_mute_rolls_the_engine_back() {
    let mut call = join(false, |_| {}).await;
    let h = call.handle.clone();
    let res = tokio::spawn(async move { h.set_media(false, true).await });
    let m = call.recv_type("call.media").await;
    call.peer
        .send(
            json!({ "type": "error", "re": m["id"], "data": { "code": "invalid", "message": "" } }),
        )
        .await;
    assert!(res.await.unwrap().is_err());
    let log = call.engine.log();
    let media: Vec<&String> = log.iter().filter(|e| e.starts_with("media:")).collect();
    assert_eq!(
        media,
        ["media:false:true", "media:true:true"],
        "not rolled back"
    );
}

/// A mute made while the publish is in flight is re-announced once the publish is applied:
/// a server may derive the announced state from the publish offer, and the user's intent must
/// win. The re-announcement is sent once.
#[tokio::test]
async fn media_intent_is_reannounced_after_publish_lands() {
    let mut call = join(true, |_| {}).await;
    let publish = call.recv_type("call.publish").await;
    let h = call.handle.clone();
    let res = tokio::spawn(async move { h.set_media(false, true).await });
    let m = call.recv_type("call.media").await;
    call.ok(&m).await;
    res.await.unwrap().unwrap();
    call.answer_publish(&publish).await;
    let again = call.recv_type("call.media").await;
    assert_eq!(
        again["data"],
        json!({ "call_id": "k1", "audio": false, "video": true })
    );
    call.ok(&again).await;
    tokio::time::sleep(QUIET).await;
    call.assert_nothing_before_leave().await;
}

#[tokio::test]
async fn no_media_frame_after_publish_without_an_intent() {
    let mut call = join(true, |_| {}).await;
    let publish = call.recv_type("call.publish").await;
    call.answer_publish(&publish).await;
    call.assert_nothing_before_leave().await;
}

/// The publish lands while a `call.media` is unanswered: one at a time, so the intent goes
/// out again only after that one is acknowledged, and only once.
#[tokio::test]
async fn media_intent_in_flight_when_publish_lands_is_sent_after_the_ack() {
    let mut call = join(true, |_| {}).await;
    let publish = call.recv_type("call.publish").await;
    let h = call.handle.clone();
    let res = tokio::spawn(async move { h.set_media(true, false).await });
    let m = call.recv_type("call.media").await;
    call.answer_publish(&publish).await;
    // One `call.media` at a time: nothing more until `m` is acknowledged.
    let early = tokio::time::timeout(QUIET, call.peer.recv()).await;
    assert!(early.is_err(), "sent before the ack: {:?}", early.ok());
    call.ok(&m).await;
    res.await.unwrap().unwrap();
    let again = call.recv_type("call.media").await;
    assert_eq!(
        again["data"],
        json!({ "call_id": "k1", "audio": true, "video": false })
    );
    call.ok(&again).await;
    tokio::time::sleep(QUIET).await;
    call.assert_nothing_before_leave().await;
}

/// An unanswered `call.media` on a live socket is not retried on that socket (its outcome is
/// unknown; the resume path re-sends it), so it cannot loop.
#[tokio::test]
async fn unanswered_media_is_not_resent_on_the_same_socket() {
    let mut call = join_with(
        true,
        |_| {},
        |c| c.with_transport(|t| t.reply_timeout = Duration::from_millis(150)),
    )
    .await;
    let publish = call.recv_type("call.publish").await;
    call.answer_publish(&publish).await;
    let h = call.handle.clone();
    let res = tokio::spawn(async move { h.set_media(false, true).await });
    call.recv_type("call.media").await; // never answered
    assert!(res.await.unwrap().is_err(), "timed out");
    tokio::time::sleep(Duration::from_millis(800)).await;
    call.assert_nothing_before_leave().await;
}

/// A re-announcement pending behind an in-flight `call.media` survives a queued newer intent
/// that the engine rejects: the engine's state is what gets announced.
#[tokio::test]
async fn pending_reannouncement_survives_a_failed_queued_intent() {
    let mut call = join(true, |_| {}).await;
    let publish = call.recv_type("call.publish").await;
    let h = call.handle.clone();
    let first = tokio::spawn(async move { h.set_media(false, true).await });
    let m = call.recv_type("call.media").await;
    call.answer_publish(&publish).await; // re-announcement now pending behind `m`
    let h = call.handle.clone();
    let queued = tokio::spawn(async move { h.set_media(true, true).await });
    tokio::time::sleep(QUIET).await; // queued behind `m`
    call.engine.fail_media.store(true, Ordering::SeqCst);
    call.ok(&m).await;
    first.await.unwrap().unwrap();
    assert!(queued.await.unwrap().is_err(), "engine refused it");
    let again = call.recv_type("call.media").await;
    assert_eq!(
        again["data"],
        json!({ "call_id": "k1", "audio": false, "video": true })
    );
}

/// The publish completion can end the call (a buffered remote candidate the engine refuses):
/// nothing is announced after `call.leave`.
#[tokio::test]
async fn no_reannouncement_after_the_call_ended() {
    let mut call = join(true, |e| e.fail_candidates.store(true, Ordering::SeqCst)).await;
    let publish = call.recv_type("call.publish").await;
    let h = call.handle.clone();
    let res = tokio::spawn(async move { h.set_media(false, true).await });
    let m = call.recv_type("call.media").await;
    call.ok(&m).await;
    res.await.unwrap().unwrap();
    // Buffered until the answer applies mid 0, then refused by the engine.
    call.peer
        .send(
            json!({ "type": "call.ice", "data": { "call_id": "k1", "pc": "publish",
            "candidate": cand("c-pub", "0") } }),
        )
        .await;
    tokio::time::sleep(QUIET).await;
    call.peer
        .send(json!({ "type": "call.publish.answer", "re": publish["id"],
            "data": { "call_id": "k1", "sdp": "pub-answer\na=mid:0" } }))
        .await;
    let leave = call.recv_type("call.leave").await;
    call.ok(&leave).await;
    call.wait_status(|s| matches!(s, CallStatus::Ended(_)))
        .await;
    let after = tokio::time::timeout(Duration::from_millis(500), call.peer.recv()).await;
    assert!(after.is_err(), "sent after leave: {:?}", after.ok());
}

#[tokio::test]
async fn unanswered_mute_is_not_rolled_back() {
    let mut call = join(false, |_| {}).await;
    let h = call.handle.clone();
    let res = tokio::spawn(async move { h.set_media(false, true).await });
    call.recv_type("call.media").await;
    call.peer.close(1000, "drop").await; // outcome unknown
    let _ = res.await;
    let log = call.engine.log();
    let media: Vec<&String> = log.iter().filter(|e| e.starts_with("media:")).collect();
    assert_eq!(
        media,
        ["media:false:true"],
        "rolled back although the server may have it"
    );
}

// ---- ending ----

/// Several end causes racing while an engine operation is held: `close()` exactly once, and
/// promptly (before the held operation is released); the held operation cannot restart capture.
#[tokio::test]
async fn racing_ends_close_once_and_fence_the_engine() {
    let mut call = join(true, |e| {
        e.gate("create_publish_offer");
    })
    .await;
    call.eventually("offer held", |l| {
        l.iter().any(|e| e == "create_publish_offer")
    })
    .await;
    call.peer
        .send(json!({ "type": "call.ended", "data": { "call_id": "k1", "reason": "sfu_restart" } }))
        .await;
    call.handle.engine_failed("boom".into());
    let _ = call.handle.leave().await;
    call.eventually("closed before release", |l| l.iter().any(|e| e == "close"))
        .await;
    call.engine.gates.lock().unwrap()["create_publish_offer"].add_permits(1);
    tokio::time::sleep(QUIET).await;
    assert_eq!(call.engine.closes.load(Ordering::SeqCst), 1);
    assert!(
        !call.engine.capture.load(Ordering::SeqCst),
        "capture restarted after close"
    );
    // The causes arrive on different channels, so any of them may win — but exactly one does.
    assert!(matches!(
        call.handle.state().borrow().status,
        CallStatus::Ended(_)
    ));
}

#[tokio::test]
async fn replaced_by_another_socket_ends_the_call() {
    let mut call = join(false, |_| {}).await;
    call.peer
        .send(json!({ "type": "call.ended", "data": { "call_id": "k1", "reason": "replaced" } }))
        .await;
    call.wait_status(|s| *s == CallStatus::Ended(EndReason::Replaced))
        .await;
}

#[tokio::test]
async fn signing_in_as_someone_else_ends_the_call() {
    let call = join(false, |_| {}).await;
    call.client.login("bob", "pw").await.unwrap();
    call.wait_status(|s| *s == CallStatus::Ended(EndReason::SessionChanged))
        .await;
    call.eventually("engine closed", |l| l.iter().any(|e| e == "close"))
        .await;
}

#[tokio::test]
async fn leave_tells_the_server_and_later_calls_fail() {
    let mut call = join(false, |_| {}).await;
    call.handle.leave().await.unwrap();
    assert_eq!(call.recv_type("call.leave").await["data"]["call_id"], "k1");
    assert!(call.handle.set_media(true, true).await.is_err());
}

/// `leave()` resolves only once the server confirmed, so an app can await it and quit
/// without racing its own leave; media stops and the status ends immediately.
#[tokio::test]
async fn leave_waits_for_the_server_before_resolving() {
    let mut call = join(false, |_| {}).await;
    let h = call.handle.clone();
    let leaving = tokio::spawn(async move { h.leave().await });
    let leave = call.recv_type("call.leave").await;
    call.wait_status(|s| *s == CallStatus::Ended(EndReason::Left))
        .await;
    call.eventually("engine closed at once", |l| l.iter().any(|e| e == "close"))
        .await;
    tokio::time::sleep(QUIET).await;
    assert!(
        !leaving.is_finished(),
        "leave() resolved before the server confirmed"
    );
    call.ok(&leave).await;
    tokio::time::timeout(WAIT, leaving)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

/// No confirmation: `leave()` still resolves, after the bounded wait.
#[tokio::test]
async fn leave_without_confirmation_resolves_after_the_bounded_wait() {
    let mut call = join(false, |_| {}).await;
    let h = call.handle.clone();
    let leaving = tokio::spawn(async move { h.leave().await });
    call.recv_type("call.leave").await; // never confirmed
    tokio::time::timeout(Duration::from_secs(5), leaving)
        .await
        .expect("leave() hung")
        .unwrap()
        .unwrap();
}

// ---- labelled offers (screen share, PROTOCOL.md §3.3) ----

const AV_SDP: &str = "v=0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=mid:0\r\na=sendonly\r\n\
                      m=video 9 UDP/TLS/RTP/SAVPF 96\r\na=mid:1\r\na=sendonly\r\n";

fn label(mid: &str, kind: &str, source: &str) -> Value {
    json!({ "mid": mid, "kind": kind, "source": source })
}

/// An engine without its own labelling: call.publish carries the default labels.
#[tokio::test]
async fn publish_carries_default_labels() {
    let mut call = join(true, |e| *e.offer_sdp.lock().unwrap() = Some(AV_SDP.into())).await;
    let publish = call.recv_type("call.publish").await;
    assert_eq!(publish["data"]["sdp"], AV_SDP);
    assert_eq!(
        publish["data"]["tracks"],
        json!([label("0", "audio", "mic"), label("1", "video", "camera")])
    );
}

/// An engine that labels a screen: its labels go out untouched.
struct Sharing(Arc<FakeEngine>);

#[async_trait]
impl MediaEngine for Sharing {
    async fn create_publish_offer(&self) -> Result<String, EngineError> {
        self.0.create_publish_offer().await
    }
    async fn create_labelled_offer(&self) -> Result<PublishOffer, EngineError> {
        let sdp = self.0.create_publish_offer().await?;
        let mut tracks = default_labels(&sdp);
        if let Some(t) = tracks.iter_mut().find(|t| t.mid == "1") {
            t.source = MediaSource::Screen;
        }
        Ok(PublishOffer { sdp, tracks })
    }
    async fn apply_publish_answer(&self, sdp: String) -> Result<(), EngineError> {
        self.0.apply_publish_answer(sdp).await
    }
    async fn apply_subscribe_offer(
        &self,
        sdp: String,
        streams: Vec<SubStream>,
    ) -> Result<String, EngineError> {
        self.0.apply_subscribe_offer(sdp, streams).await
    }
    fn add_remote_candidate(&self, pc: PcKind, c: Option<IceCandidate>) -> Result<(), EngineError> {
        self.0.add_remote_candidate(pc, c)
    }
    fn set_local_media(&self, audio: bool, video: bool) -> Result<(), EngineError> {
        self.0.set_local_media(audio, video)
    }
    async fn close(&self) {
        self.0.close().await
    }
}

#[tokio::test]
async fn engine_labels_go_out_as_given() {
    let mut server = TestServer::start().await;
    let (client, mut peer) = connected(&mut server).await;
    let fake = Arc::new(FakeEngine::default());
    *fake.offer_sdp.lock().unwrap() = Some(AV_SDP.into());
    let engine = Arc::new(Sharing(fake));
    let c = client.clone();
    let joining = tokio::spawn(async move { c.join_call("ch", engine, true).await });
    let f = peer.recv().await;
    peer.send(joined(&f["id"], "t1")).await;
    let _handle = joining.await.unwrap().unwrap();
    let publish = loop {
        let f = peer.recv().await;
        if f["type"] == "call.publish" {
            break f;
        }
    };
    assert_eq!(
        publish["data"]["tracks"],
        json!([label("0", "audio", "mic"), label("1", "video", "screen")])
    );
}

/// An offer made while the socket is down goes out after resume with its labels.
#[tokio::test]
async fn resent_offer_keeps_its_labels() {
    let mut call = join(true, |e| {
        *e.offer_sdp.lock().unwrap() = Some(AV_SDP.into());
        e.gate("create_publish_offer");
    })
    .await;
    let resume = call.reconnect().await;
    // The offer completes while the call is resuming: it is kept and sent after resume.
    call.engine.gates.lock().unwrap()["create_publish_offer"].add_permits(1);
    tokio::time::sleep(QUIET).await;
    call.peer.send(joined(&resume["id"], "t2")).await;
    let publish = call.recv_type("call.publish").await;
    assert_eq!(
        publish["data"]["tracks"],
        json!([label("0", "audio", "mic"), label("1", "video", "camera")])
    );
}

#[test]
fn default_labels_cover_every_media_mline() {
    let sdp = "v=0\r\n\
               m=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=mid:a\r\n\
               m=video 0 UDP/TLS/RTP/SAVPF 96\r\na=mid:v\r\na=inactive\r\n\
               m=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\na=mid:d\r\n\
               m=video 9 UDP/TLS/RTP/SAVPF 96\r\na=sendonly\r\n";
    let got: Vec<(String, MediaKind, MediaSource)> = default_labels(sdp)
        .into_iter()
        .map(|t| (t.mid, t.kind, t.source))
        .collect();
    assert_eq!(
        got,
        vec![
            ("a".into(), MediaKind::Audio, MediaSource::Mic),
            ("v".into(), MediaKind::Video, MediaSource::Camera), // inactive: still labelled
            ("3".into(), MediaKind::Video, MediaSource::Camera), // no a=mid: its position
        ]
    );
}
