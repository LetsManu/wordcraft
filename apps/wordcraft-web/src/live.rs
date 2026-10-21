//! OfficeHub: edit a document together, live.
//!
//! The portal starts the app with `?collab=<url>` (same site, ticketed). The page says hello there and
//! learns whether it starts the session (seeds it with the document it opened) or joins it (gets
//! everything changed so far). After that it keeps one request open for the others' changes and
//! presence, sends its own changes in order, and tells the others where its caret is.
//! The engine does the merging (`Session::collab_*`); this is only the transport.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

use base64::Engine as _;
use serde_json::{Value, json};
use wasm_bindgen::JsCast as _;
use wasm_bindgen::JsValue;
use wordcraft_ui_egui::{RemotePeer, WordApp};

const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;

#[derive(Default)]
struct Shared {
    /// From hello.
    peer_id: Option<String>,
    slot: u32,
    seed: bool,
    /// Updates that arrived (hello's log first), in order.
    inbound: Vec<Vec<u8>>,
    /// Whether the session was joined or seeded (only then are changes sent or merged).
    connected: bool,
    /// Asked to compact: the number of log entries the state should replace.
    compact: Option<u64>,
    /// Asked to seed (the first person left before doing it).
    reseed: bool,
    peers: BTreeMap<String, RemotePeer>,
    /// (slot, may save) of everyone else, to agree on who saves automatically.
    slots: BTreeMap<String, (u32, bool)>,
    /// Someone changed the presentation since the last automatic save (time of the last change).
    changed_at: Option<f64>,
    saving: bool,
    /// Waiting to be sent, in order; one request at a time.
    outbox: Vec<Vec<u8>>,
    sending: bool,
    failed: bool,
    gone: bool,
}

/// The live session of this page.
pub struct Live {
    url: String,
    /// Where an automatic save goes (`None`: this person may only look).
    save: Option<String>,
    shared: Rc<RefCell<Shared>>,
    started: bool,
    last_presence: String,
    last_presence_at: f64,
    /// The session's document when the page started: the portal's file has arrived once it differs.
    initial_document: u64,
}

impl Live {
    /// The session the portal asked for, if any.
    pub fn from_portal(app: &WordApp) -> Option<Self> {
        let url = crate::portal::param("collab").filter(|u| u.starts_with('/') && !u.starts_with("//"))?;
        let save = crate::portal::save_url().map(|u| format!("{u}&auto=1"));
        Some(Live {
            url,
            save,
            shared: Rc::default(),
            started: false,
            last_presence: String::new(),
            last_presence_at: 0.0,
            initial_document: app.session.document_id(),
        })
    }

    fn endpoint(&self, action: &str) -> String {
        match self.url.split_once('?') {
            Some((base, query)) => format!("{base}/{action}?{query}"),
            None => format!("{}/{action}", self.url),
        }
    }

    /// Run every frame (after the app's own logic).
    pub fn frame(&mut self, app: &mut WordApp, ctx: &egui::Context) {
        // Wait for the portal's document before connecting it.
        if app.session.document_id() == self.initial_document {
            return;
        }
        if !self.started {
            self.started = true;
            self.hello(ctx);
            self.bye_on_close();
            return;
        }
        let (connected, peer_id) = {
            let s = self.shared.borrow();
            (s.connected, s.peer_id.clone())
        };
        let Some(peer_id) = peer_id else { return };
        if !connected {
            self.connect(app);
        }
        let now = ctx.input(|i| i.time);
        let mut s = self.shared.borrow_mut();
        if !s.connected {
            return;
        }
        // Others' changes.
        let inbound = std::mem::take(&mut s.inbound);
        let compact = s.compact.take();
        app.peers = s.peers.values().cloned().collect();
        drop(s);
        let had_inbound = !inbound.is_empty();
        for u in inbound {
            if let Err(e) = app.session.collab_incoming(&u) {
                log::error!("live editing: {e}");
            }
        }
        if had_inbound {
            self.shared.borrow_mut().changed_at = Some(now);
        }
        if let (Some(upto), Some(state)) = (compact, app.session.collab_state()) {
            let body = json!({"peer_id": peer_id, "data": B64.encode(state), "upto": upto});
            post(self.endpoint("compact"), body, |_| {});
        }
        // Our changes.
        if let Some(u) = app.session.collab_outgoing() {
            let mut s = self.shared.borrow_mut();
            s.outbox.push(u);
            s.changed_at = Some(now);
        }
        self.pump(ctx);
        self.autosave(app, ctx, now);
        // Where we are, for the others (at most every 150 ms, only when it changed).
        {
            let state = json!({"caret": app.session.sel.focus});
            let text = state.to_string();
            if text != self.last_presence && now - self.last_presence_at > 0.15 {
                self.last_presence = text;
                self.last_presence_at = now;
                post(self.endpoint("presence"), json!({"peer_id": peer_id, "state": state}), |_| {});
            }
        }
        let mut s = self.shared.borrow_mut();
        if std::mem::take(&mut s.gone) {
            app.status("The live session ended. Please open the presentation again from OfficeHub.");
        }
        if std::mem::take(&mut s.failed) {
            app.status("A change could not be sent to the others. Please check the connection.");
        }
    }

    /// Everyone's changes are saved to the file by one person: whoever may save and has the lowest
    /// slot, a few seconds after the last change. OfficeHub keeps these saves in one version.
    fn autosave(&mut self, app: &mut WordApp, ctx: &egui::Context, now: f64) {
        const QUIET: f64 = 3.0;
        let Some(save) = self.save.clone() else { return };
        {
            let s = self.shared.borrow();
            let Some(at) = s.changed_at else { return };
            let leader = s.slots.values().filter(|(_, may)| *may).all(|(slot, _)| *slot > s.slot);
            if s.saving || !leader || now - at < QUIET {
                if s.changed_at.is_some() && !s.saving {
                    ctx.request_repaint_after(std::time::Duration::from_millis(500));
                }
                return;
            }
        }
        let Some(target) = crate::portal::portal_extension() else { return };
        let bytes = match wordcraft_engine::io::save_bytes(&format!("document.{target}"), &app.session.doc) {
            Ok(b) => b,
            Err(e) => {
                log::error!("automatic save: {e}");
                return;
            }
        };
        app.session.dirty = false;
        {
            let mut s = self.shared.borrow_mut();
            s.saving = true;
            s.changed_at = None;
        }
        let shared = self.shared.clone();
        let ctx = ctx.clone();
        wasm_bindgen_futures::spawn_local(async move {
            if let Err(e) = crate::portal::fetch(&save, "PUT", Some(&bytes)).await {
                log::error!("automatic save: {e}");
                // Try again after the next quiet moment.
                shared.borrow_mut().changed_at = Some(0.0);
            }
            shared.borrow_mut().saving = false;
            ctx.request_repaint();
        });
    }

    /// Seed or join once hello has answered (a joiner waits for the first update).
    fn connect(&mut self, app: &mut WordApp) {
        let mut s = self.shared.borrow_mut();
        if s.seed || s.reseed {
            s.reseed = false;
            let update = app.session.collab_seed(client_id());
            s.outbox.push(update);
            s.connected = true;
        } else if !s.inbound.is_empty() {
            let log = std::mem::take(&mut s.inbound);
            match app.session.collab_join(client_id(), &log) {
                Ok(()) => s.connected = true,
                Err(e) => log::error!("live editing: {e}"),
            }
        }
    }

    fn hello(&self, ctx: &egui::Context) {
        let shared = self.shared.clone();
        let poll_url = self.endpoint("poll");
        let ctx = ctx.clone();
        let hello = self.endpoint("hello");
        wasm_bindgen_futures::spawn_local(async move {
            let reply = match request(&hello, &json!({})).await {
                Ok(v) => v,
                Err(e) => {
                    log::error!("live editing unavailable: {e}");
                    return;
                }
            };
            {
                let mut s = shared.borrow_mut();
                s.peer_id = reply["peer_id"].as_str().map(String::from);
                s.slot = reply["slot"].as_u64().and_then(|n| u32::try_from(n).ok()).unwrap_or(0);
                s.seed = reply["seed"].as_bool().unwrap_or(false);
                for u in reply["log"].as_array().into_iter().flatten() {
                    if let Some(bytes) = u.as_str().and_then(|t| B64.decode(t).ok()) {
                        s.inbound.push(bytes);
                    }
                }
                for p in reply["peers"].as_array().into_iter().flatten() {
                    if let Some(id) = p["id"].as_str() {
                        s.peers.insert(id.to_string(), peer_of(p));
                        s.slots.insert(id.to_string(), slot_of(p));
                    }
                }
            }
            ctx.request_repaint();
            // Keep one request open for everything that happens.
            let mut after = 0u64;
            loop {
                let Some(peer_id) = shared.borrow().peer_id.clone() else { return };
                match request(&poll_url, &json!({"peer_id": peer_id, "after": after})).await {
                    Ok(v) => {
                        let mut s = shared.borrow_mut();
                        for m in v["messages"].as_array().into_iter().flatten() {
                            after = after.max(m["seq"].as_u64().unwrap_or(0));
                            let from = m["from"].as_str().unwrap_or_default().to_string();
                            match m["type"].as_str().unwrap_or_default() {
                                "update" => {
                                    if let Some(bytes) = m["data"].as_str().and_then(|t| B64.decode(t).ok()) {
                                        s.inbound.push(bytes);
                                    }
                                }
                                "join" => {
                                    s.slots.insert(from.clone(), slot_of(&m["data"]));
                                    s.peers.insert(from, peer_of(&m["data"]));
                                }
                                "leave" => {
                                    s.peers.remove(&from);
                                    s.slots.remove(&from);
                                }
                                "presence" => {
                                    if let Some(p) = s.peers.get_mut(&from) {
                                        apply_presence(p, &m["data"]);
                                    }
                                }
                                "compact" => s.compact = m["data"]["upto"].as_u64(),
                                "seed" => s.reseed = true,
                                _ => {}
                            }
                        }
                        drop(s);
                        ctx.request_repaint();
                    }
                    Err(e) if e.contains("410") => {
                        shared.borrow_mut().gone = true;
                        ctx.request_repaint();
                        return;
                    }
                    Err(e) => {
                        log::warn!("live editing: {e}; trying again");
                        sleep(2000).await;
                    }
                }
            }
        });
    }

    /// Send the queued changes, one request at a time, in order.
    fn pump(&self, ctx: &egui::Context) {
        let (body, peer_id) = {
            let mut s = self.shared.borrow_mut();
            if s.sending || s.outbox.is_empty() {
                return;
            }
            let Some(peer_id) = s.peer_id.clone() else { return };
            let all = std::mem::take(&mut s.outbox);
            s.sending = true;
            (wordcraft_engine::collab::merge(&all), peer_id)
        };
        let shared = self.shared.clone();
        let url = self.endpoint("update");
        let ctx = ctx.clone();
        wasm_bindgen_futures::spawn_local(async move {
            let payload = json!({"peer_id": peer_id, "data": B64.encode(&body)});
            let mut tries = 0;
            loop {
                match request(&url, &payload).await {
                    Ok(_) => break,
                    Err(e) if e.contains("403") || e.contains("410") || tries >= 5 => {
                        log::error!("live editing: a change could not be sent: {e}");
                        shared.borrow_mut().failed = true;
                        break;
                    }
                    Err(_) => {
                        tries += 1;
                        sleep(500 * tries).await;
                    }
                }
            }
            shared.borrow_mut().sending = false;
            ctx.request_repaint();
        });
    }

    /// Leave the session when the page closes.
    fn bye_on_close(&self) {
        let shared = self.shared.clone();
        let url = self.endpoint("bye");
        let closure = wasm_bindgen::closure::Closure::<dyn FnMut()>::new(move || {
            if let Some(peer_id) = shared.borrow().peer_id.clone() {
                let _ = send(&url, &json!({"peer_id": peer_id}), true);
            }
        });
        if let Some(w) = web_sys::window() {
            let _ = w.add_event_listener_with_callback("pagehide", closure.as_ref().unchecked_ref());
        }
        closure.forget();
        // Changes not saved yet (the automatic save waits for a quiet moment): ask before leaving.
        // OfficeHub asks the page the same way before going back.
        let shared = self.shared.clone();
        let unsaved = wasm_bindgen::closure::Closure::<dyn FnMut(web_sys::Event)>::new(move |e: web_sys::Event| {
            let s = shared.borrow();
            if s.changed_at.is_some() || s.saving || !s.outbox.is_empty() || s.sending {
                e.prevent_default();
            }
        });
        if let Some(w) = web_sys::window() {
            let _ = w.add_event_listener_with_callback("beforeunload", unsaved.as_ref().unchecked_ref());
        }
        unsaved.forget();
    }
}

fn slot_of(v: &Value) -> (u32, bool) {
    (v["slot"].as_u64().and_then(|n| u32::try_from(n).ok()).unwrap_or(u32::MAX), v["can_write"].as_bool().unwrap_or(false))
}

fn peer_of(v: &Value) -> RemotePeer {
    let mut p = RemotePeer {
        name: v["name"].as_str().unwrap_or("?").to_string(),
        color: parse_color(v["color"].as_str().unwrap_or("#2563eb")),
        ..Default::default()
    };
    apply_presence(&mut p, &v["state"]);
    p
}

fn apply_presence(p: &mut RemotePeer, state: &Value) {
    p.caret = serde_json::from_value(state["caret"].clone()).ok();
}

fn parse_color(hex: &str) -> [u8; 3] {
    let h = hex.trim_start_matches('#');
    let c = |i: usize| h.get(i..i + 2).and_then(|s| u8::from_str_radix(s, 16).ok()).unwrap_or(0x60);
    [c(0), c(2), c(4)]
}

/// A random CRDT client id (53 bits, as Yjs expects).
fn client_id() -> u64 {
    let hi = (js_sys::Math::random() * f64::from(1u32 << 21)) as u64;
    let lo = (js_sys::Math::random() * f64::from(u32::MAX)) as u64;
    ((hi << 32) | lo) & ((1 << 53) - 1)
}

fn js(e: JsValue) -> String {
    format!("{e:?}")
}

/// Start a JSON POST (with `keepalive` when the page is closing).
fn send(url: &str, body: &Value, keepalive: bool) -> Result<js_sys::Promise, String> {
    let window = web_sys::window().ok_or("no window")?;
    let init = js_sys::Object::new();
    js_sys::Reflect::set(&init, &"method".into(), &"POST".into()).map_err(js)?;
    js_sys::Reflect::set(&init, &"credentials".into(), &"same-origin".into()).map_err(js)?;
    js_sys::Reflect::set(&init, &"keepalive".into(), &keepalive.into()).map_err(js)?;
    let headers = js_sys::Object::new();
    js_sys::Reflect::set(&headers, &"X-Requested-With".into(), &"OfficeHub".into()).map_err(js)?;
    js_sys::Reflect::set(&headers, &"Content-Type".into(), &"application/json".into()).map_err(js)?;
    js_sys::Reflect::set(&init, &"headers".into(), &headers).map_err(js)?;
    js_sys::Reflect::set(&init, &"body".into(), &body.to_string().into()).map_err(js)?;
    let fetch_fn: js_sys::Function =
        js_sys::Reflect::get(&window, &"fetch".into()).map_err(js)?.dyn_into().map_err(|_| "fetch is not available".to_string())?;
    fetch_fn.call2(&window, &url.into(), &init).map_err(js)?.dyn_into().map_err(|_| "not a promise".to_string())
}

/// POST JSON and read the JSON answer (`null` for 204).
async fn request(url: &str, body: &Value) -> Result<Value, String> {
    let response = wasm_bindgen_futures::JsFuture::from(send(url, body, false)?).await.map_err(js)?;
    let status = js_sys::Reflect::get(&response, &"status".into()).ok().and_then(|v| v.as_f64()).unwrap_or(0.0) as u32;
    if !(200..300).contains(&status) {
        return Err(format!("HTTP {status}"));
    }
    if status == 204 {
        return Ok(Value::Null);
    }
    let text_fn: js_sys::Function = js_sys::Reflect::get(&response, &"text".into()).map_err(js)?.dyn_into().map_err(|_| "no text()".to_string())?;
    let promise: js_sys::Promise = text_fn.call0(&response).map_err(js)?.dyn_into().map_err(|_| "not a promise".to_string())?;
    let text = wasm_bindgen_futures::JsFuture::from(promise).await.map_err(js)?.as_string().unwrap_or_default();
    serde_json::from_str(&text).map_err(|e| e.to_string())
}

/// Fire and forget.
fn post(url: String, body: Value, done: impl FnOnce(Result<Value, String>) + 'static) {
    wasm_bindgen_futures::spawn_local(async move {
        done(request(&url, &body).await);
    });
}

async fn sleep(ms: u32) {
    let promise = js_sys::Promise::new(&mut |resolve, _| {
        if let Some(w) = web_sys::window() {
            let _ = w.set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, ms as i32);
        }
    });
    let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
}
