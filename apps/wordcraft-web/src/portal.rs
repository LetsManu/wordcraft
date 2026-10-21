//! OfficeHub: open a document from the portal and save it back there, like Word in SharePoint.
//!
//! The portal starts the app with `?src=<url>&save=<url>&name=<file name>`: both URLs are on the same
//! site and carry a short-lived ticket. Opening fetches `src`; saving the document under its own name
//! sends the bytes to `save` (HTTP PUT), and the portal keeps them as the file's next version. Without
//! these parameters the app behaves exactly as before.
#![allow(dead_code)] // shared by WordCraft and DeckCraft; each uses part of it

use wasm_bindgen::JsCast as _;
use wasm_bindgen::JsValue;

/// A query parameter, URL-decoded.
pub fn param(key: &str) -> Option<String> {
    let search = web_sys::window().and_then(|w| w.location().search().ok()).unwrap_or_default();
    for pair in search.trim_start_matches('?').split('&') {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        if k == key {
            return js_sys::decode_uri_component(v).ok().map(String::from);
        }
    }
    None
}

/// Only same-site paths: the app never sends a document anywhere else.
fn same_site(url: &str) -> bool {
    url.starts_with('/') && !url.starts_with("//")
}

/// The document the portal asked to open, as (URL, file name).
pub fn source() -> Option<(String, String)> {
    let src = param("src").filter(|s| same_site(s))?;
    Some((src, param("name").unwrap_or_else(|| "document".into())))
}

fn extension(name: &str) -> String {
    std::path::Path::new(name).extension().and_then(|e| e.to_str()).unwrap_or_default().to_ascii_lowercase()
}

/// Where a save of `path` goes back to the portal: only the opened file itself (same file type);
/// exports such as PDF or pictures still download.
pub fn save_target(path: &str) -> Option<String> {
    let save = param("save").filter(|s| same_site(s))?;
    let name = param("name")?;
    (extension(path) == extension(&name)).then_some(save)
}

/// The file type of the opened portal file ("docx", "pptx", …).
pub fn portal_extension() -> Option<String> {
    param("save").filter(|s| same_site(s))?;
    param("name").map(|n| extension(&n))
}

/// The ticketed save URL, whatever the name of the save.
pub fn save_url() -> Option<String> {
    param("save").filter(|s| same_site(s))
}

fn js(e: JsValue) -> String {
    format!("{e:?}")
}

/// GET or PUT `url` on the portal; the response body.
pub async fn fetch(url: &str, method: &str, body: Option<&[u8]>) -> Result<Vec<u8>, String> {
    let window = web_sys::window().ok_or("no window")?;
    let init = js_sys::Object::new();
    js_sys::Reflect::set(&init, &"method".into(), &method.into()).map_err(js)?;
    js_sys::Reflect::set(&init, &"credentials".into(), &"same-origin".into()).map_err(js)?;
    let headers = js_sys::Object::new();
    js_sys::Reflect::set(&headers, &"X-Requested-With".into(), &"OfficeHub".into()).map_err(js)?;
    js_sys::Reflect::set(&init, &"headers".into(), &headers).map_err(js)?;
    if let Some(bytes) = body {
        js_sys::Reflect::set(&init, &"body".into(), &js_sys::Uint8Array::from(bytes)).map_err(js)?;
    }
    let fetch_fn: js_sys::Function =
        js_sys::Reflect::get(&window, &"fetch".into()).map_err(js)?.dyn_into().map_err(|_| "fetch is not available".to_string())?;
    let promise: js_sys::Promise = fetch_fn.call2(&window, &url.into(), &init).map_err(js)?.dyn_into().map_err(|_| "not a promise".to_string())?;
    let response = wasm_bindgen_futures::JsFuture::from(promise).await.map_err(js)?;
    let status = js_sys::Reflect::get(&response, &"status".into()).ok().and_then(|v| v.as_f64()).unwrap_or(0.0) as u32;
    if !(200..300).contains(&status) {
        return Err(match status {
            401 | 403 => "OfficeHub: the link has expired. Please open the document again from OfficeHub.".to_string(),
            409 => "OfficeHub: someone else is editing this file right now.".to_string(),
            423 => "OfficeHub: someone else has checked out this file.".to_string(),
            413 => "OfficeHub: the file is too large.".to_string(),
            _ => format!("OfficeHub: saving failed (HTTP {status})."),
        });
    }
    let buffer_fn: js_sys::Function =
        js_sys::Reflect::get(&response, &"arrayBuffer".into()).map_err(js)?.dyn_into().map_err(|_| "no arrayBuffer".to_string())?;
    let buffer_promise: js_sys::Promise = buffer_fn.call0(&response).map_err(js)?.dyn_into().map_err(|_| "not a promise".to_string())?;
    let buffer = wasm_bindgen_futures::JsFuture::from(buffer_promise).await.map_err(js)?;
    Ok(js_sys::Uint8Array::new(&buffer).to_vec())
}
