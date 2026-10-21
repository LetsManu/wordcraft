//! Live co-editing in the session (see `wordcraft_collab`).
//!
//! The host (the web app inside OfficeHub) connects the document to a shared session: the first
//! person [seeds](Session::collab_seed) it, everyone else [joins](Session::collab_join). After
//! that the host sends what [`Session::collab_outgoing`] returns and hands every update from the
//! others to [`Session::collab_incoming`]. Commands, typing and the UI work as before; undo and
//! redo take back only this person's own steps.

use std::sync::Arc;

use wordcraft_collab::Collab;
use wordcraft_doc::Document;

use crate::session::Session;

pub use wordcraft_collab::merge;

/// The document's co-editing state. A copy of a session does not take part.
#[derive(Default)]
pub struct Shared {
    pub(crate) collab: Option<Box<Collab>>,
    /// Updates made by commands (undo, finished steps) waiting for the host to send them.
    pub(crate) outbox: Vec<Vec<u8>>,
    /// The session revision the shared document last saw.
    pub(crate) synced_rev: u64,
}

impl std::fmt::Debug for Shared {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Shared").field("active", &self.collab.is_some()).finish()
    }
}

impl Shared {
    pub fn is_active(&self) -> bool {
        self.collab.is_some()
    }
}

impl Session {
    /// Whether the document is shared right now.
    pub fn collab_active(&self) -> bool {
        self.shared.is_active()
    }

    /// Write local edits to the shared document (once per change of the session revision).
    fn collab_flush(&mut self, force: bool) {
        let rev = self.rev();
        let caret = Some(self.sel.focus.clone());
        let Some(c) = self.shared.collab.as_mut() else { return };
        if !force && self.shared.synced_rev == rev {
            return;
        }
        c.set_caret(caret);
        if let Some(u) = c.local(&Arc::new(self.doc.clone())) {
            self.shared.outbox.push(u);
        }
        self.shared.synced_rev = rev;
    }

    /// End an undo step in a shared session (called when the next one begins).
    pub(crate) fn collab_end_step(&mut self) {
        if self.shared.collab.is_none() {
            return;
        }
        self.collab_flush(false);
        if let Some(c) = self.shared.collab.as_mut() {
            c.step();
        }
    }

    /// Undo or redo in a shared session; `None` without one.
    pub(crate) fn collab_undo(&mut self, redo: bool) -> Option<bool> {
        self.shared.collab.as_ref()?;
        self.collab_flush(false);
        let current = Arc::new(self.doc.clone());
        let c = self.shared.collab.as_mut()?;
        match c.undo(&current, redo) {
            Ok(Some((d, update))) => {
                self.shared.outbox.push(update);
                self.adopt_shared(d);
                Some(true)
            }
            Ok(None) => Some(false),
            Err(e) => {
                log::error!("shared undo: {e}");
                Some(false)
            }
        }
    }

    pub(crate) fn collab_can_undo(&self) -> Option<bool> {
        self.shared.collab.as_ref().map(|c| c.can_undo())
    }

    pub(crate) fn collab_can_redo(&self) -> Option<bool> {
        self.shared.collab.as_ref().map(|c| c.can_redo())
    }

    /// Show the shared document, keeping the selection where the text around it moved to.
    fn adopt_shared(&mut self, d: Arc<Document>) {
        self.doc = Arc::unwrap_or_clone(d);
        self.doc.ensure_nonempty();
        self.touch();
        self.shared.synced_rev = self.rev();
        self.clamp_selection();
    }

    /// Start a shared session with the document as it is (the first person). Returns the update
    /// describing it, for the server to hand to everyone who joins.
    pub fn collab_seed(&mut self, client: u64) -> Vec<u8> {
        let mut c = Box::new(Collab::new(client));
        let update = c.seed(&self.doc);
        c.local(&Arc::new(self.doc.clone()));
        c.step();
        self.reset_history();
        self.shared = Shared { collab: Some(c), outbox: Vec::new(), synced_rev: self.rev() };
        update
    }

    /// Join a shared session: the document becomes the shared one.
    pub fn collab_join(&mut self, client: u64, updates: &[Vec<u8>]) -> Result<(), String> {
        let mut c = Box::new(Collab::new(client));
        let d = c.join(updates).map_err(|e| e.to_string())?;
        self.set_document(d);
        self.shared = Shared { collab: Some(c), outbox: Vec::new(), synced_rev: self.rev() };
        Ok(())
    }

    pub fn collab_leave(&mut self) {
        self.shared = Shared::default();
    }

    /// What to send to the others now, if anything. Call it every frame.
    pub fn collab_outgoing(&mut self) -> Option<Vec<u8>> {
        self.shared.collab.as_ref()?;
        self.collab_flush(false);
        let out = std::mem::take(&mut self.shared.outbox);
        (!out.is_empty()).then(|| merge(&out))
    }

    /// Merge an update from someone else.
    pub fn collab_incoming(&mut self, update: &[u8]) -> Result<(), String> {
        if self.shared.collab.is_none() {
            return Ok(());
        }
        // Our own edits first, so the selection below is counted in the text as we see it.
        self.collab_flush(false);
        let current = Arc::new(self.doc.clone());
        let Some(c) = self.shared.collab.as_mut() else { return Ok(()) };
        let anchors = c.anchors(&current, &[&self.sel.anchor, &self.sel.focus]);
        let (d, pending) = c.remote(update, &current).map_err(|e| e.to_string())?;
        if let Some(u) = pending {
            self.shared.outbox.push(u);
        }
        if *d == *current {
            return Ok(());
        }
        let moved = c.positions(&d, &anchors);
        self.adopt_shared(d);
        if let [Some(anchor), Some(focus)] = &moved[..] {
            self.sel = crate::session::Selection { anchor: anchor.clone(), focus: focus.clone() };
            self.clamp_selection();
        }
        Ok(())
    }

    /// The whole shared document as one update.
    pub fn collab_state(&self) -> Option<Vec<u8>> {
        self.shared.collab.as_ref().map(|c| c.state())
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::sample::sample_document;

    fn two() -> (Session, Session) {
        let mut a = Session::new(sample_document());
        let seed = a.collab_seed(11);
        let mut b = Session::new(Document::new());
        b.collab_join(12, &[seed]).unwrap();
        (a, b)
    }

    fn sync(from: &mut Session, to: &mut Session) {
        if let Some(u) = from.collab_outgoing() {
            to.collab_incoming(&u).unwrap();
        }
    }

    #[test]
    fn the_sample_handbook_arrives_exactly() {
        let (a, b) = two();
        assert_eq!(a.doc.body, b.doc.body);
        assert_eq!(a.doc.parts, b.doc.parts);
        assert_eq!(a.doc.styles, b.doc.styles);
        assert_eq!(a.doc.core, b.doc.core);
    }

    #[test]
    fn typing_together_in_one_paragraph() {
        let (mut a, mut b) = two();
        a.run("select.text", &json!({"text": "Every Thursday"})).unwrap();
        b.run("select.text", &json!({"text": "people who make things"})).unwrap();
        a.run("text.insert", &json!({"text": "On"})).unwrap();
        b.run("text.insert", &json!({"text": "makers"})).unwrap();
        sync(&mut a, &mut b);
        sync(&mut b, &mut a);
        sync(&mut a, &mut b);
        assert_eq!(a.doc.body, b.doc.body);
        let text = a.doc.plain_text(wordcraft_doc::StoryRef::Body);
        assert!(text.contains("On evening") && text.contains("fills with makers"), "{text}");
        // B keeps typing where B was.
        b.run("text.insert", &json!({"text": " and friends"})).unwrap();
        sync(&mut b, &mut a);
        assert!(a.doc.plain_text(wordcraft_doc::StoryRef::Body).contains("makers and friends"));
    }

    #[test]
    fn undo_takes_back_only_my_step() {
        let (mut a, mut b) = two();
        a.run("select.text", &json!({"text": "Painters"})).unwrap();
        a.run("text.insert", &json!({"text": "Sculptors"})).unwrap();
        sync(&mut a, &mut b);
        b.run("select.text", &json!({"text": "letterpress"})).unwrap();
        b.run("text.insert", &json!({"text": "linocut"})).unwrap();
        sync(&mut b, &mut a);
        assert!(a.run("edit.undo", &json!({})).is_ok());
        sync(&mut a, &mut b);
        assert_eq!(a.doc.body, b.doc.body);
        let text = b.doc.plain_text(wordcraft_doc::StoryRef::Body);
        assert!(text.contains("Painters") && text.contains("linocut"), "{text}");
    }
}
