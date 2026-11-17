//! EffectCraft hosted inside Workbench's side panel: the same app as the desktop build, drawn into a view
//! Workbench owns, with Workbench's look. A video and effects editor always shows its full tools (panels,
//! timeline, viewer), so there is no Hands off mode. Workbench (and Claude) drive it through its control
//! channel. See `craft-embed`.
//!
//! The app's `eframe::App` impl is driven as eframe drives it, with an [`eframe::Frame`] that carries the
//! host's GPU (the viewer and compositor draw with wgpu). Requests go through the app's own control channel,
//! so the replies it gives frames later (synthetic input, retries, background jobs) arrive through
//! [`EmbedApp::settle`].

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

use std::sync::mpsc::{Receiver, Sender, TryRecvError};

use craft_embed::{EmbedApp, egui, egui_wgpu};
use effectcraft_ui_egui::{ControlRequest, EffectcraftApp};
use serde_json::{Value, json};

struct Effect {
    app: EffectcraftApp,
    frame: eframe::Frame,
    /// The app's control channel (it answers requests in its logic tick).
    requests: Sender<ControlRequest>,
    /// The reply of a request the app answers on a later frame.
    pending: Option<Receiver<Value>>,
}

impl EmbedApp for Effect {
    fn logic(&mut self, ctx: &egui::Context) {
        eframe::App::logic(&mut self.app, ctx, &mut self.frame);
    }
    fn raw_input_hook(&mut self, ctx: &egui::Context, raw: &mut egui::RawInput) {
        eframe::App::raw_input_hook(&mut self.app, ctx, raw);
    }
    fn ui(&mut self, ui: &mut egui::Ui) {
        eframe::App::ui(&mut self.app, ui, &mut self.frame);
    }
    fn control(&mut self, ctx: &egui::Context, request: &str) -> String {
        let req: Value = serde_json::from_str(request).unwrap_or(Value::Null);
        let method = req.get("method").and_then(Value::as_str).unwrap_or("");
        let params = req.get("params").cloned().unwrap_or(json!({}));
        if method == "ui.screenshot" {
            return json!({"ok": false, "error": "Workbench takes the screenshot of the panel itself"}).to_string();
        }
        let (method, params) = translate(method, params);
        let (req, rx) = ControlRequest::new(method, params);
        if self.requests.send(req).is_err() {
            return json!({"ok": false, "error": "the app's control channel is closed"}).to_string();
        }
        // The app answers in its logic tick; run it now (as eframe does while the window is hidden), so a
        // request it answers at once has its reply before this returns.
        eframe::App::logic(&mut self.app, ctx, &mut self.frame);
        match rx.try_recv() {
            Ok(v) => v.to_string(),
            Err(TryRecvError::Empty) => {
                // answered over the next frames (the host runs them and asks `settle`)
                self.pending = Some(rx);
                json!({"ok": true, "result": {"pending": true}}).to_string()
            }
            Err(TryRecvError::Disconnected) => json!({"ok": false, "error": "the app dropped the request"}).to_string(),
        }
    }
    fn settle(&mut self) -> Option<String> {
        let reply = match self.pending.as_ref()?.try_recv() {
            Ok(v) => v,
            Err(TryRecvError::Empty) => return None,
            Err(TryRecvError::Disconnected) => json!({"ok": false, "error": "the app dropped the request"}),
        };
        self.pending = None;
        Some(reply.to_string())
    }
    fn gpu(&mut self, rs: &egui_wgpu::RenderState) {
        // What the desktop's app creator gets as `cc.wgpu_render_state`: the app builds its compositor and
        // display pass on it in its first tick.
        self.frame.wgpu_render_state = Some(rs.clone());
    }
    fn exit(&mut self) {
        eframe::App::on_exit(&mut self.app);
    }
}

/// The Workbench protocol's methods as EffectCraft's control channel names them. Files go by their real
/// path: the engine reads and writes them with the native file system.
fn translate(method: &str, params: Value) -> (String, Value) {
    let path = || json!({"path": params.get("path").cloned().unwrap_or(Value::Null)});
    let execute = |command: &str, p: Value| ("engine.execute".to_string(), json!({"command": command, "params": p}));
    match method {
        "file.open" => execute("file.open", path()),
        "file.saveAs" => execute("file.saveAs", path()),
        "file.save" => execute("file.save", params),
        "file.new" => execute("file.newProject", json!({})),
        "document.inspect" => execute("project.summary", json!({})),
        "file.import" => import(&params),
        "engine.execute" => match params.get("command").and_then(Value::as_str) {
            Some("file.new") => execute("file.newProject", json!({})),
            // one file by path (Workbench adding media), as a file dropped on the app: a project opens,
            // media goes into the open project (EffectCraft always has one, untitled when new)
            Some("file.import") if params.pointer("/params/path").is_some() => import(params.get("params").unwrap_or(&Value::Null)),
            _ => ("engine.execute".to_string(), params),
        },
        other => (other.to_string(), params),
    }
}

/// `{path}` imported as a file dropped on the app (the web build's `addFile`): an `.ecproj` opens, anything
/// else (video, audio, image) becomes footage in the open project.
fn import(params: &Value) -> (String, Value) {
    let path = params.get("path").and_then(Value::as_str).unwrap_or("");
    let (command, p) =
        if path.to_ascii_lowercase().ends_with(".ecproj") { ("file.open", json!({"path": path})) } else { ("file.import", json!({"paths": [path]})) };
    ("engine.execute".to_string(), json!({"command": command, "params": p}))
}

fn make(ctx: &egui::Context) -> Box<dyn EmbedApp> {
    EffectcraftApp::install_ime_reset_hooks(ctx);
    // No settings folder, crash recovery or startup scripts: Workbench owns the files.
    let session = effectcraft_host::session();
    let mut app = EffectcraftApp::new(session);
    app.set_startup_update_check(false);
    app.ui.start_screen = false;
    let (requests, rx) = std::sync::mpsc::channel();
    app = app.with_control(rx);
    // Workbench's look from the first frame; Workbench sets the mode's accent right after
    let _ = effectcraft_ui_egui::menus::invoke(&mut app, ctx, "view.workbench", json!({"on": true}));
    Box::new(Effect { app, frame: eframe::Frame::_new_kittest(), requests, pending: None })
}

craft_embed::export_embed!(make);
