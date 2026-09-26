//! The in-call view: a grid of video tiles, a self-view, and the call controls
//! (mic, camera, hang up).
//!
//! The view knows nothing about signaling. It renders what the media engine
//! (`brook-media-gst`) hands it: each video sink is a `gtk4paintablesink`, whose
//! `paintable` a `gtk::Picture` draws. Whoever owns the call (the core
//! `CallHandle`, or the dev loopback below) feeds tiles in and reacts to the
//! control callbacks.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use adw::prelude::*;
use brook_core::{BrookClient, CallHandle, CallState, CallStatus, EndReason, MediaEngine};
use brook_media_gst::{
    CameraSource, EngineConfig, EngineEvent, GstEngine, MediaSource, MicSource, PcKind,
    SinkFactory, TrackKind, VideoCodec,
};
use gtk::{gdk, glib};
use tokio::runtime::Handle;

/// A `gtk4paintablesink` for every video stream (remote tiles and self-view).
pub fn paintable_sink_factory() -> SinkFactory {
    Arc::new(|kind| match kind {
        TrackKind::Video => gst::ElementFactory::make("gtk4paintablesink")
            .build()
            .expect("gtk4paintablesink (gst-plugin-gtk4) is installed"),
        TrackKind::Audio => gst::ElementFactory::make("autoaudiosink")
            .build()
            .expect("autoaudiosink is installed"),
    })
}

/// Read the `paintable` of a `gtk4paintablesink`. Must run on the GTK thread.
fn paintable_of(sink: &gst::Element) -> Option<gdk::Paintable> {
    sink.find_property("paintable")
        .map(|_| sink.property::<gdk::Paintable>("paintable"))
}

/// Engine configuration from the environment (dev knobs until a settings UI):
/// `BROOK_CAMERA` = `auto` (default) | `test` | `none` | a V4L2 device path,
/// `BROOK_MIC` = `auto` | `test` | `none`, `BROOK_VIDEO_CODEC` = `h264` | `vp8`,
/// `BROOK_HW_ENCODE=1` to try VA-API first.
///
/// Fails (instead of panicking later on a GStreamer thread) when the sinks
/// the call view renders into aren't installed.
pub fn engine_config_from_env() -> Result<EngineConfig, String> {
    gst::init().map_err(|e| format!("GStreamer: {e}"))?;
    for (element, package) in [
        (
            "gtk4paintablesink",
            "the GStreamer GTK 4 plugin (gst-plugin-gtk4)",
        ),
        ("autoaudiosink", "GStreamer good plugins"),
    ] {
        if gst::ElementFactory::find(element).is_none() {
            return Err(format!("{element} is missing: install {package}"));
        }
    }
    let camera = match std::env::var("BROOK_CAMERA").as_deref() {
        Ok("test") => CameraSource::Test,
        Ok("none") => CameraSource::None,
        Ok(path) if path.starts_with('/') => CameraSource::Device(path.to_string()),
        _ => CameraSource::Auto,
    };
    let mic = match std::env::var("BROOK_MIC").as_deref() {
        Ok("test") => MicSource::Test,
        Ok("none") => MicSource::None,
        _ => MicSource::Auto,
    };
    let codec = match std::env::var("BROOK_VIDEO_CODEC").as_deref() {
        Ok("vp8") => VideoCodec::Vp8,
        _ => VideoCodec::H264,
    };
    Ok(EngineConfig {
        camera,
        mic,
        codec,
        hardware_encode: std::env::var("BROOK_HW_ENCODE").as_deref() == Ok("1"),
        video_kbps: 1500,
        ice_servers: Vec::new(),
        video_sink: paintable_sink_factory(),
        audio_sink: None,
    })
}

/// The call view's widgets and tiles. Cheap to clone (all GObject refs).
#[derive(Clone)]
pub struct CallView {
    root: gtk::Widget,
    grid: gtk::FlowBox,
    grid_stack: gtk::Stack,
    self_view: gtk::Picture,
    status: adw::Banner,
    mic_button: gtk::ToggleButton,
    camera_button: gtk::ToggleButton,
    share_button: gtk::ToggleButton,
    hangup_button: gtk::Button,
    tiles: Rc<RefCell<HashMap<String, Tile>>>,
}

#[derive(Clone)]
struct Tile {
    child: gtk::FlowBoxChild,
    picture: gtk::Picture,
    label: gtk::Label,
}

impl CallView {
    /// Build an empty call view.
    pub fn new(title: &str) -> Self {
        let grid = gtk::FlowBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .homogeneous(true)
            .min_children_per_line(1)
            .max_children_per_line(3)
            .row_spacing(6)
            .column_spacing(6)
            .margin_top(6)
            .margin_bottom(6)
            .margin_start(6)
            .margin_end(6)
            .valign(gtk::Align::Center)
            .build();

        let empty = adw::StatusPage::builder()
            .icon_name("call-start-symbolic")
            .title("Waiting for others")
            .description("Their video appears here when they join.")
            .build();
        let grid_stack = gtk::Stack::new();
        grid_stack.add_named(&empty, Some("empty"));
        grid_stack.add_named(&grid, Some("grid"));
        grid_stack.set_vexpand(true);

        // Self-view: small, bottom-right over the grid.
        let self_view = gtk::Picture::builder()
            .content_fit(gtk::ContentFit::Cover)
            .width_request(200)
            .height_request(112)
            .halign(gtk::Align::End)
            .valign(gtk::Align::End)
            .margin_end(12)
            .margin_bottom(12)
            .build();
        self_view.add_css_class("card");
        self_view.set_overflow(gtk::Overflow::Hidden);
        let overlay = gtk::Overlay::new();
        overlay.set_child(Some(&grid_stack));
        overlay.add_overlay(&self_view);

        let mic_button = control_toggle("microphone-sensitivity-high-symbolic", "Mute microphone");
        let camera_button = control_toggle("camera-web-symbolic", "Turn camera off");
        let share_button = control_toggle("video-display-symbolic", "Share your screen");
        let hangup_button = gtk::Button::builder()
            .icon_name("call-stop-symbolic")
            .tooltip_text("Leave call")
            .build();
        hangup_button.add_css_class("circular");
        hangup_button.add_css_class("destructive-action");
        hangup_button.set_size_request(48, 48);

        let controls = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(18)
            .halign(gtk::Align::Center)
            .margin_top(12)
            .margin_bottom(12)
            .build();
        controls.append(&mic_button);
        controls.append(&camera_button);
        controls.append(&share_button);
        controls.append(&hangup_button);

        let status = adw::Banner::new("");

        let header = adw::HeaderBar::new();
        header.set_title_widget(Some(&adw::WindowTitle::new(title, "Call")));
        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&header);
        toolbar.add_top_bar(&status);
        toolbar.set_content(Some(&overlay));
        toolbar.add_bottom_bar(&controls);

        let view = Self {
            root: toolbar.upcast(),
            grid,
            grid_stack,
            self_view,
            status,
            mic_button,
            camera_button,
            share_button,
            hangup_button,
            tiles: Rc::default(),
        };
        // Sharing is shown as an accent-coloured toggle.
        view.share_button.connect_toggled(|b| {
            b.set_tooltip_text(Some(if b.is_active() {
                "Stop sharing your screen"
            } else {
                "Share your screen"
            }));
            if b.is_active() {
                b.add_css_class("suggested-action");
            } else {
                b.remove_css_class("suggested-action");
            }
        });

        // Icons and tooltips follow the toggle state (active = muted / off).
        view.mic_button.connect_toggled(|b| {
            let (icon, tip) = if b.is_active() {
                ("microphone-disabled-symbolic", "Unmute microphone")
            } else {
                ("microphone-sensitivity-high-symbolic", "Mute microphone")
            };
            b.set_icon_name(icon);
            b.set_tooltip_text(Some(tip));
        });
        let self_view = view.self_view.clone();
        view.camera_button.connect_toggled(move |b| {
            let (icon, tip) = if b.is_active() {
                ("camera-disabled-symbolic", "Turn camera on")
            } else {
                ("camera-web-symbolic", "Turn camera off")
            };
            b.set_icon_name(icon);
            b.set_tooltip_text(Some(tip));
            self_view.set_opacity(if b.is_active() { 0.3 } else { 1.0 });
        });
        view
    }

    /// The top-level widget.
    pub fn widget(&self) -> &gtk::Widget {
        &self.root
    }

    /// Show the local camera in the self-view.
    pub fn set_self_view(&self, sink: &gst::Element) {
        self.self_view.set_paintable(paintable_of(sink).as_ref());
    }

    /// Add (or re-bind) the video tile for a remote stream. A shared screen
    /// gets a large tile at the front of the grid.
    pub fn set_remote_video(&self, mid: &str, sink: &gst::Element, name: &str, screen: bool) {
        let paintable = paintable_of(sink);
        let mut tiles = self.tiles.borrow_mut();
        let tile = tiles.entry(mid.to_string()).or_insert_with(|| {
            let tile = new_tile();
            if screen {
                self.grid.prepend(&tile.child);
            } else {
                self.grid.append(&tile.child);
            }
            tile
        });
        let (w, h) = if screen { (640, 360) } else { (320, 180) };
        tile.picture.set_size_request(w, h);
        tile.picture.set_paintable(paintable.as_ref());
        tile.label.set_text(name);
        drop(tiles);
        self.refresh_empty();
    }

    /// Called with the desired state when the user toggles screen sharing.
    pub fn connect_share_toggled(&self, f: impl Fn(bool) + 'static) {
        self.share_button.connect_toggled(move |b| f(b.is_active()));
    }

    /// Reflect the actual sharing state (e.g. the picker was cancelled)
    /// without re-triggering the toggle handler's action.
    pub fn set_sharing(&self, on: bool, handler_guard: &std::cell::Cell<bool>) {
        handler_guard.set(true);
        self.share_button.set_active(on);
        handler_guard.set(false);
    }

    /// Rename a tile (e.g. once the roster maps its mid to a participant).
    pub fn set_tile_name(&self, mid: &str, name: &str) {
        if let Some(tile) = self.tiles.borrow().get(mid) {
            tile.label.set_text(name);
        }
    }

    /// Remove a remote stream's tile (it left the latest subscribe offer).
    pub fn remove_tile(&self, mid: &str) {
        if let Some(tile) = self.tiles.borrow_mut().remove(mid) {
            self.grid.remove(&tile.child);
        }
        self.refresh_empty();
    }

    /// Show a status line (connecting, reconnecting, errors); empty hides it.
    pub fn set_status(&self, text: &str) {
        self.status.set_title(text);
        self.status.set_revealed(!text.is_empty());
    }

    /// Grey out (and show as off) the mic / camera when there is none to
    /// publish, so toggles never ask the engine to enable a missing track.
    pub fn set_available(&self, mic: bool, camera: bool) {
        for (button, available) in [(&self.mic_button, mic), (&self.camera_button, camera)] {
            if !available {
                button.set_active(true);
                button.set_sensitive(false);
            }
        }
    }

    /// Called with `(audio_on, video_on)` whenever the user toggles mic/camera.
    pub fn connect_media_toggled(&self, f: impl Fn(bool, bool) + 'static) {
        let f = Rc::new(f);
        let mic = self.mic_button.downgrade();
        let cam = self.camera_button.downgrade();
        let emit = Rc::new(move || {
            if let (Some(mic), Some(cam)) = (mic.upgrade(), cam.upgrade()) {
                f(!mic.is_active(), !cam.is_active());
            }
        });
        let e = emit.clone();
        self.mic_button.connect_toggled(move |_| e());
        self.camera_button.connect_toggled(move |_| emit());
    }

    /// Called when the user hangs up.
    pub fn connect_hangup(&self, f: impl Fn() + 'static) {
        self.hangup_button.connect_clicked(move |_| f());
    }

    /// Mids that currently have a tile.
    fn tile_mids(&self) -> Vec<String> {
        self.tiles.borrow().keys().cloned().collect()
    }

    fn refresh_empty(&self) {
        let name = if self.tiles.borrow().is_empty() {
            "empty"
        } else {
            "grid"
        };
        self.grid_stack.set_visible_child_name(name);
    }
}

fn control_toggle(icon: &str, tooltip: &str) -> gtk::ToggleButton {
    let b = gtk::ToggleButton::builder()
        .icon_name(icon)
        .tooltip_text(tooltip)
        .build();
    b.add_css_class("circular");
    b.set_size_request(48, 48);
    b
}

fn new_tile() -> Tile {
    let picture = gtk::Picture::builder()
        .content_fit(gtk::ContentFit::Contain)
        .width_request(320)
        .height_request(180)
        .build();
    let label = gtk::Label::builder()
        .halign(gtk::Align::Start)
        .valign(gtk::Align::End)
        .margin_start(8)
        .margin_bottom(8)
        .build();
    label.add_css_class("osd");
    label.add_css_class("caption");
    let overlay = gtk::Overlay::new();
    overlay.set_child(Some(&picture));
    overlay.add_overlay(&label);
    overlay.add_css_class("card");
    overlay.set_overflow(gtk::Overflow::Hidden);
    let child = gtk::FlowBoxChild::new();
    child.set_child(Some(&overlay));
    Tile {
        child,
        picture,
        label,
    }
}

/// Dev-only (`BROOK_CALL_LOOPBACK=1`): a call with yourself, no server. One
/// engine publishes your camera + mic, a second subscribes to it, with SDP and
/// ICE passed directly between them the way `core` will pass them through the
/// SFU. Remote audio goes to a fakesink so the loop doesn't howl.
pub fn present_loopback(app: &adw::Application, runtime: &Handle) {
    let view = CallView::new("Loopback");
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Brook - loopback call")
        .default_width(960)
        .default_height(640)
        .content(view.widget())
        .build();
    window.present();

    let mut pub_config = match engine_config_from_env() {
        Ok(config) => config,
        Err(err) => {
            view.set_status(&format!("Media engine unavailable: {err}"));
            return;
        }
    };
    let mut sub_config = pub_config.clone();
    sub_config.audio_sink = Some(Arc::new(|_| {
        gst::ElementFactory::make("fakesink")
            .property("sync", false)
            .build()
            .expect("fakesink")
    }));
    pub_config.audio_sink = None;

    let engines =
        GstEngine::new(pub_config).and_then(|p| GstEngine::new(sub_config).map(|s| (p, s)));
    let ((publisher, mut pub_events), (subscriber, mut sub_events)) = match engines {
        Ok(e) => e,
        Err(err) => {
            view.set_status(&format!("Media engine unavailable: {err}"));
            return;
        }
    };
    view.set_status("Connecting...");

    // Controls.
    view.connect_media_toggled({
        let publisher = publisher.clone();
        move |audio, video| {
            if let Err(err) = publisher.set_local_media(audio, video) {
                tracing::warn!(%err, "set_local_media");
            }
        }
    });
    view.connect_hangup({
        let (publisher, subscriber) = (publisher.clone(), subscriber.clone());
        let window = window.downgrade();
        move || {
            publisher.close();
            subscriber.close();
            if let Some(w) = window.upgrade() {
                w.close();
            }
        }
    });

    // Publisher events -> UI + subscriber ICE.
    glib::spawn_future_local({
        let view = view.clone();
        let subscriber = subscriber.clone();
        async move {
            while let Some(ev) = pub_events.recv().await {
                match ev {
                    EngineEvent::LocalPreview { sink } => view.set_self_view(&sink),
                    EngineEvent::LocalCandidate { candidate, .. } => {
                        if let Err(err) =
                            subscriber.add_remote_candidate(PcKind::Subscribe, candidate.as_ref())
                        {
                            tracing::warn!(%err, "loopback: subscriber ICE candidate");
                        }
                    }
                    EngineEvent::Error { message, .. } => view.set_status(&message),
                    _ => {}
                }
            }
        }
    });
    // Subscriber events -> UI + publisher ICE.
    glib::spawn_future_local({
        let view = view.clone();
        let publisher = publisher.clone();
        async move {
            while let Some(ev) = sub_events.recv().await {
                match ev {
                    EngineEvent::RemoteTrack {
                        mid,
                        kind: TrackKind::Video,
                        sink,
                    } => {
                        view.set_remote_video(&mid, &sink, "You (via loopback)", false);
                    }
                    EngineEvent::LocalCandidate { candidate, .. } => {
                        if let Err(err) =
                            publisher.add_remote_candidate(PcKind::Publish, candidate.as_ref())
                        {
                            tracing::warn!(%err, "loopback: publisher ICE candidate");
                        }
                    }
                    EngineEvent::ConnectionState { state, .. } => {
                        use gst_webrtc::WebRTCPeerConnectionState as S;
                        match state {
                            S::Connected => view.set_status(""),
                            S::Failed => view.set_status("Connection failed"),
                            S::Disconnected => view.set_status("Reconnecting..."),
                            _ => {}
                        }
                    }
                    EngineEvent::Error { message, .. } => view.set_status(&message),
                    _ => {}
                }
            }
        }
    });

    // Negotiate on the runtime (the engine's awaits use Tokio timers).
    let runtime = runtime.clone();
    glib::spawn_future_local(async move {
        let result = runtime
            .spawn(async move {
                let offer = publisher.create_publish_offer().await?;
                let answer = subscriber.apply_subscribe_offer(&offer, vec![]).await?;
                publisher.apply_publish_answer(&answer).await
            })
            .await;
        match result {
            Ok(Ok(())) => {}
            Ok(Err(err)) => view.set_status(&format!("Negotiation failed: {err}")),
            Err(err) => view.set_status(&format!("Negotiation task failed: {err}")),
        }
    });
}

/// Why a call ended, for the status banner.
fn end_text(reason: &EndReason) -> String {
    match reason {
        EndReason::Left => "You left the call.".into(),
        EndReason::SfuRestart => "The call ended: the media server restarted.".into(),
        EndReason::Removed => "You were removed from this channel.".into(),
        // Left the channel (maybe from another device): its call ends too.
        EndReason::LeftChannel => "You left this channel, so its call ended.".into(),
        EndReason::Replaced => "You joined this call from another window or device.".into(),
        EndReason::Expired => "Lost connection to the call.".into(),
        EndReason::SessionChanged => "You were signed out.".into(),
        EndReason::EngineFailed(msg) => format!("Camera/microphone failure: {msg}"),
        EndReason::Server(code) => match code.as_str() {
            "call_full" => "This call is full.".into(),
            "sfu_unavailable" => "Calls are unavailable right now.".into(),
            _ => format!("The server ended the call ({code})."),
        },
        _ => "The call ended.".into(),
    }
}

/// Open a call window for `channel_id` and join its call through core.
/// Closing the window (or hanging up) leaves the call.
pub fn open_call(
    parent: Option<&gtk::Window>,
    client: Arc<BrookClient>,
    runtime: Handle,
    channel_id: String,
    title: &str,
) -> adw::Window {
    let view = CallView::new(title);
    let window = adw::Window::builder()
        .title(format!("Call - {title}"))
        .default_width(960)
        .default_height(640)
        .content(view.widget())
        .build();
    window.set_transient_for(parent);
    window.present();

    let config = match engine_config_from_env() {
        Ok(c) => c,
        Err(err) => {
            view.set_status(&format!("Media engine unavailable: {err}"));
            return window;
        }
    };
    // Before the toggle handlers are wired: this is initial state, not a toggle.
    view.set_available(
        config.mic != MicSource::None,
        config.camera != CameraSource::None,
    );
    let (engine, mut engine_events) = match GstEngine::new(config) {
        Ok(e) => e,
        Err(err) => {
            view.set_status(&format!("Media engine unavailable: {err}"));
            return window;
        }
    };
    view.set_status("Joining...");

    // The handle, once joined; dropped (-> leave) when the window goes away.
    let handle: Rc<RefCell<Option<Arc<CallHandle>>>> = Rc::default();
    // mid -> (participant id, is a shared screen), from the latest applied
    // subscribe offer.
    let mids: Rc<RefCell<HashMap<String, (String, bool)>>> = Rc::default();
    // The latest call state (roster names).
    let state: Rc<RefCell<Option<CallState>>> = Rc::default();

    let name_of = {
        let (mids, state) = (mids.clone(), state.clone());
        move |mid: &str| -> String {
            let Some((pid, screen)) = mids.borrow().get(mid).cloned() else {
                return String::new();
            };
            let name = state
                .borrow()
                .as_ref()
                .and_then(|s| {
                    s.participants
                        .iter()
                        .find(|p| p.participant_id == pid)
                        .map(|p| p.display_name.clone())
                })
                .unwrap_or_default();
            if screen {
                format!("{name} (screen)")
            } else {
                name
            }
        }
    };
    let name_of = Rc::new(name_of);

    // Controls.
    view.connect_media_toggled({
        let (handle, runtime) = (handle.clone(), runtime.clone());
        move |audio, video| {
            if let Some(h) = handle.borrow().clone() {
                runtime.spawn(async move {
                    if let Err(err) = h.set_media(audio, video).await {
                        tracing::warn!(%err, "set_media");
                    }
                });
            }
        }
    });
    // Screen share: the desktop's picker, then a new m-line on the publish
    // PC and a republish; off stops it and republishes. `quiet` suppresses the
    // handler while the UI is reset programmatically.
    let quiet = Rc::new(std::cell::Cell::new(false));
    view.connect_share_toggled({
        let (view, handle, runtime, engine, quiet) = (
            view.clone(),
            handle.clone(),
            runtime.clone(),
            engine.clone(),
            quiet.clone(),
        );
        move |on| {
            if quiet.get() {
                return;
            }
            let Some(call) = handle.borrow().clone() else {
                view.set_sharing(false, &quiet);
                return;
            };
            let (view, runtime, engine, quiet) =
                (view.clone(), runtime.clone(), engine.clone(), quiet.clone());
            glib::spawn_future_local(async move {
                let result = if on {
                    match runtime.spawn(brook_media_gst::request_screen_cast()).await {
                        Ok(Ok(source)) => {
                            engine.start_screen_share(source).map_err(|e| e.to_string())
                        }
                        Ok(Err(err)) => Err(err.to_string()),
                        Err(err) => Err(err.to_string()),
                    }
                } else {
                    stop_share(&engine, &runtime)
                };
                match result {
                    Ok(()) => {
                        // The republish's own result, not just the task's.
                        let republished = runtime
                            .spawn(async move { call.republish().await })
                            .await
                            .map_err(|e| e.to_string())
                            .and_then(|r| r.map_err(|e| e.to_string()));
                        if let Err(err) = republished {
                            tracing::warn!(%err, "republish after screen share toggle");
                            if on {
                                // Nobody will see this share: undo it.
                                let _ = stop_share(&engine, &runtime);
                                view.set_sharing(false, &quiet);
                                view.set_status("Couldn't share the screen");
                            }
                        }
                    }
                    Err(err) => {
                        tracing::info!(%err, "screen share not started");
                        view.set_sharing(false, &quiet);
                    }
                }
            });
        }
    });

    view.connect_hangup({
        let window = window.downgrade();
        move || {
            if let Some(w) = window.upgrade() {
                w.close();
            }
        }
    });
    window.connect_close_request({
        let (handle, runtime) = (handle.clone(), runtime.clone());
        move |_| {
            if let Some(h) = handle.borrow_mut().take() {
                runtime.spawn(async move {
                    let _ = h.leave().await;
                });
            }
            glib::Propagation::Proceed
        }
    });

    // Join, then pump engine events into core and the view. Events emitted
    // before the handle exists (publish candidates) wait in the channel.
    glib::spawn_future_local({
        let (view, handle, mids, state, name_of) = (
            view.clone(),
            handle.clone(),
            mids.clone(),
            state.clone(),
            name_of.clone(),
        );
        let quiet_events = quiet.clone();
        let window = window.downgrade();
        async move {
            let engine_dyn: Arc<dyn MediaEngine> = engine.clone();
            let joined = runtime
                .spawn({
                    let client = client.clone();
                    async move { client.join_call(&channel_id, engine_dyn, true).await }
                })
                .await;
            let call = match joined {
                Ok(Ok(call)) => call,
                Ok(Err(err)) => {
                    engine.close();
                    view.set_status(&format!("Couldn't join the call: {err}"));
                    return;
                }
                Err(err) => {
                    engine.close();
                    view.set_status(&format!("Couldn't join the call: {err}"));
                    return;
                }
            };
            // The window may have been closed while joining: leave at once.
            if window.upgrade().is_none() {
                runtime.spawn(async move {
                    let _ = call.leave().await;
                });
                return;
            }
            *handle.borrow_mut() = Some(call.clone());
            watch_call_state(&view, &call, &state, &name_of);

            while let Some(ev) = engine_events.recv().await {
                match ev {
                    EngineEvent::LocalCandidate { pc, candidate } => {
                        call.local_candidate(pc, candidate)
                    }
                    EngineEvent::LocalPreview { sink } => view.set_self_view(&sink),
                    EngineEvent::SubscribeStreams(streams) => {
                        let video: HashMap<String, (String, bool)> = streams
                            .iter()
                            .map(|s| {
                                (
                                    s.mid.clone(),
                                    (s.participant_id.clone(), s.source == MediaSource::Screen),
                                )
                            })
                            .collect();
                        *mids.borrow_mut() = video;
                        // Tiles for mids no longer in the offer are gone.
                        for mid in view.tile_mids() {
                            if !mids.borrow().contains_key(&mid) {
                                view.remove_tile(&mid);
                            } else {
                                view.set_tile_name(&mid, &name_of(&mid));
                            }
                        }
                    }
                    EngineEvent::RemoteTrack {
                        mid,
                        kind: TrackKind::Video,
                        sink,
                    } => {
                        let screen = mids.borrow().get(&mid).is_some_and(|(_, screen)| *screen);
                        view.set_remote_video(&mid, &sink, &name_of(&mid), screen)
                    }
                    // The desktop ended our share (its "stop sharing" button,
                    // or the window closed): stop it here too, and republish.
                    EngineEvent::ScreenShareEnded { message } => {
                        tracing::info!(%message, "screen share ended by the desktop");
                        // The share is over either way: reset the button first.
                        view.set_sharing(false, &quiet_events);
                        if stop_share(&engine, &runtime).is_ok() {
                            let call = call.clone();
                            runtime.spawn(async move { call.republish().await });
                        }
                    }
                    EngineEvent::Error { message, .. } => {
                        tracing::warn!(%message, "media engine error");
                        call.engine_failed(message);
                    }
                    _ => {}
                }
            }
        }
    });
    window
}

/// Stop the engine's share and close the portal session (on the runtime).
fn stop_share(engine: &Arc<GstEngine>, runtime: &Handle) -> Result<(), String> {
    match engine.stop_screen_share() {
        Ok(Some(session)) => {
            runtime.spawn(session.close());
            Ok(())
        }
        Ok(None) => Ok(()),
        Err(err) => Err(err.to_string()),
    }
}

/// Reflect core's call state: status banner, tile names, end of call.
fn watch_call_state(
    view: &CallView,
    call: &Arc<CallHandle>,
    state: &Rc<RefCell<Option<CallState>>>,
    name_of: &Rc<impl Fn(&str) -> String + 'static>,
) {
    let mut rx = call.state();
    let (view, state, name_of) = (view.clone(), state.clone(), name_of.clone());
    glib::spawn_future_local(async move {
        loop {
            let current = rx.borrow_and_update().clone();
            match &current.status {
                CallStatus::Joining => view.set_status("Joining..."),
                CallStatus::Connected => view.set_status(""),
                CallStatus::Reconnecting => view.set_status("Reconnecting..."),
                CallStatus::Ended(reason) => view.set_status(&end_text(reason)),
            }
            let ended = matches!(current.status, CallStatus::Ended(_));
            *state.borrow_mut() = Some(current);
            for mid in view.tile_mids() {
                view.set_tile_name(&mid, &name_of(&mid));
            }
            if ended || rx.changed().await.is_err() {
                break;
            }
        }
    });
}
