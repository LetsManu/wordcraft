//! Live co-editing: several people work on one document at the same time.
//!
//! The document lives in a Yjs document ([yrs]) every participant holds a replica of. The engine
//! keeps editing its own [`Document`] as before; this crate translates between the two:
//! [`Collab::local`] writes what changed since the last sync, [`Collab::remote`] merges another
//! participant's update and rebuilds only the stories it touched.
//!
//! | Root | Key | Value |
//! |---|---|---|
//! | `meta` (map) | a `Document` field (camelCase) | that field as JSON |
//! | `stories` (map) | `body`, a part id | the story's text ([`story`]) |
//! | `parts` (map) | part id | the part without its blocks, as JSON |
//! | `cells` (map) | cell id | a table cell's text ([`story`]) |
//! | `comments` (map) | comment id | the comment as JSON |
//! | `media` (map) | media key | the bytes |
//!
//! Undo and redo take back only this participant's own steps.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]
#![forbid(unsafe_code)]

mod story;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use serde_json::Value;
use wordcraft_doc::{Blocks, Document, Part, Pos, StoryRef};
use yrs::undo::{Options as UndoOptions, UndoManager};
use yrs::updates::decoder::Decode;
use yrs::{Any, Doc, Map, MapRef, Origin, Out, ReadTxn, StateVector, StickyIndex, TextPrelim, TextRef, Transact, TransactionMut, Update};

const LOCAL: &str = "wordcraft-local";

#[derive(Debug, thiserror::Error)]
pub enum CollabError {
    #[error("the update from the other participants could not be read: {0}")]
    Update(String),
    #[error("the shared document could not be read: {0}")]
    Shape(String),
}

pub type Result<T> = std::result::Result<T, CollabError>;

/// A caret or selection end that moves with the text when others type before it.
#[derive(Clone, Debug)]
pub struct Anchor {
    story: StoryRef,
    /// The table cell whose text holds it (`None`: the story's own paragraphs).
    cell: Option<String>,
    sticky: StickyIndex,
}

/// One participant's replica of the shared document.
pub struct Collab {
    doc: Doc,
    meta: MapRef,
    stories: MapRef,
    parts: MapRef,
    cells: MapRef,
    comments: MapRef,
    media: MapRef,
    client: u64,
    next_cell: u64,
    synced: Option<Arc<Document>>,
    seen: HashMap<String, Arc<str>>,
    undo: UndoManager<()>,
    caret: Option<Pos>,
}

impl std::fmt::Debug for Collab {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Collab").finish_non_exhaustive()
    }
}

fn story_key(s: StoryRef) -> String {
    match s {
        StoryRef::Body => "body".into(),
        StoryRef::Part(id) => id.to_string(),
    }
}

fn blocks_of(d: &Document, s: StoryRef) -> Option<&Blocks> {
    match s {
        StoryRef::Body => Some(&d.body),
        StoryRef::Part(id) => d.parts.get(&id).map(|p| &p.blocks),
    }
}

/// The document's own fields shared as `meta` entries.
fn meta_fields(d: &Document) -> serde_json::Map<String, Value> {
    let mut q = d.clone();
    q.body = Vec::new();
    q.parts = BTreeMap::new();
    q.comments = BTreeMap::new();
    q.media = BTreeMap::new();
    match serde_json::to_value(&q) {
        Ok(Value::Object(mut m)) => {
            for k in ["body", "parts", "comments", "media"] {
                m.remove(k);
            }
            m
        }
        _ => serde_json::Map::new(),
    }
}

/// Whether the document's own fields are the same (compared without copying or serialising them).
fn same_meta(a: &Document, b: &Document) -> bool {
    a.last_section == b.last_section
        && a.styles == b.styles
        && a.numbering == b.numbering
        && a.revisions == b.revisions
        && a.settings == b.settings
        && a.core == b.core
        && a.sources == b.sources
        && a.custom_props == b.custom_props
        && a.footnote_separators == b.footnote_separators
        && a.endnote_separators == b.endnote_separators
        && a.passthrough == b.passthrough
        && a.glossary == b.glossary
}

fn part_json(p: &Part) -> Option<String> {
    let q = Part { blocks: Vec::new(), ..p.clone() };
    serde_json::to_string(&q).ok()
}

impl Collab {
    /// A replica; `client_id` identifies it in the CRDT and must be unique (random is fine).
    pub fn new(client_id: u64) -> Self {
        let doc = Doc::with_client_id(client_id & ((1 << 53) - 1));
        let mut undo = UndoManager::with_options(UndoOptions {
            capture_timeout_millis: u64::MAX,
            tracked_origins: [Origin::from(LOCAL)].into_iter().collect(),
            capture_transaction: None,
            timestamp: Arc::new(|| 1u64),
            init_undo_stack: Vec::new(),
            init_redo_stack: Vec::new(),
        });
        for name in ["meta", "stories", "parts", "cells", "comments", "media"] {
            undo.expand_scope(&doc, &doc.get_or_insert_map(name));
        }
        Collab {
            meta: doc.get_or_insert_map("meta"),
            stories: doc.get_or_insert_map("stories"),
            parts: doc.get_or_insert_map("parts"),
            cells: doc.get_or_insert_map("cells"),
            comments: doc.get_or_insert_map("comments"),
            media: doc.get_or_insert_map("media"),
            client: client_id & ((1 << 53) - 1),
            next_cell: 0,
            doc,
            synced: None,
            seen: HashMap::new(),
            undo,
            caret: None,
        }
    }

    /// The first participant fills the shared document with the document it opened.
    pub fn seed(&mut self, d: &Document) -> Vec<u8> {
        self.synced = Some(Arc::new(Document { body: Vec::new(), ..Document::new() }));
        let update = self.local(&Arc::new(d.clone())).unwrap_or_default();
        self.undo.clear_all();
        self.step();
        update
    }

    /// Join a session: merge everything so far and build the document.
    pub fn join(&mut self, updates: &[Vec<u8>]) -> Result<Document> {
        {
            let mut txn = self.doc.transact_mut();
            for u in updates {
                let update = Update::decode_v1(u).map_err(|e| CollabError::Update(e.to_string()))?;
                txn.apply_update(update).map_err(|e| CollabError::Update(e.to_string()))?;
            }
        }
        let d = Arc::new(self.materialize(&Document::new())?);
        self.synced = Some(d.clone());
        Ok(Arc::unwrap_or_clone(d))
    }

    pub fn state(&self) -> Vec<u8> {
        self.doc.transact().encode_state_as_update_v1(&StateVector::default())
    }

    /// Where this person's caret is, so an ambiguous change (a space next to a space) lands there.
    pub fn set_caret(&mut self, caret: Option<Pos>) {
        self.caret = caret;
    }

    pub fn step(&mut self) {
        self.undo.reset();
    }

    pub fn can_undo(&self) -> bool {
        self.undo.can_undo()
    }

    pub fn can_redo(&self) -> bool {
        self.undo.can_redo()
    }

    /// Write what changed since the last sync. `None`: nothing to send.
    pub fn local(&mut self, d: &Arc<Document>) -> Option<Vec<u8>> {
        let old = self.synced.clone().unwrap_or_default();
        if Arc::ptr_eq(&old, d) {
            return None;
        }
        let before = self.doc.transact().state_vector();
        {
            let doc = self.doc.clone();
            let mut txn = doc.transact_mut_with(LOCAL);
            if !self.seen.contains_key("meta:styles") || !same_meta(&old, d) {
                self.write_meta(&mut txn, d);
            }
            self.write_story(&mut txn, StoryRef::Body, &old.body, &d.body);
            self.write_parts(&mut txn, &old, d);
            self.write_comments(&mut txn, &old, d);
            self.write_media(&mut txn, &old, d);
        }
        self.synced = Some(d.clone());
        let update = self.doc.transact().encode_diff_v1(&before);
        (update.len() > 2).then_some(update)
    }

    fn put(&mut self, txn: &mut TransactionMut, map: &MapRef, kind: &str, key: &str, json: String) {
        let value: Arc<str> = Arc::from(json);
        let seen_key = format!("{kind}:{key}");
        if self.seen.get(&seen_key).is_some_and(|s| *s == value) {
            return;
        }
        map.insert(txn, key, Any::String(value.clone()));
        self.seen.insert(seen_key, value);
    }

    fn write_meta(&mut self, txn: &mut TransactionMut, d: &Document) {
        let meta = self.meta.clone();
        for (k, v) in meta_fields(d) {
            if let Ok(s) = serde_json::to_string(&v) {
                self.put(txn, &meta, "meta", &k, s);
            }
        }
    }

    fn write_story(&mut self, txn: &mut TransactionMut, s: StoryRef, old: &Blocks, new: &Blocks) {
        let key = story_key(s);
        let caret = self.caret.clone().filter(|c| c.story == s);
        let caret = caret.as_ref().map(|c| (c.path.0.as_slice(), c.off));
        let mut cx = story::Ctx { cells: &self.cells, client: self.client, next: &mut self.next_cell };
        match self.stories.get(txn, &key) {
            Some(Out::YText(t)) => story::write(txn, &t, old, new, caret, &mut cx, 0),
            _ => {
                let t: TextRef = self.stories.insert(txn, key.as_str(), TextPrelim::new(""));
                story::write(txn, &t, &Vec::new(), new, None, &mut cx, 0);
            }
        }
    }

    fn write_parts(&mut self, txn: &mut TransactionMut, old: &Document, new: &Document) {
        let parts = self.parts.clone();
        for (id, p) in &new.parts {
            let prev = old.parts.get(id);
            if let Some(json) = part_json(p) {
                self.put(txn, &parts, "part", &id.to_string(), json);
            }
            let empty = Vec::new();
            let old_blocks = prev.map(|q| &q.blocks).unwrap_or(&empty);
            if prev.is_none() || old_blocks != &p.blocks {
                self.write_story(txn, StoryRef::Part(*id), old_blocks, &p.blocks);
            }
        }
        for id in old.parts.keys().filter(|id| !new.parts.contains_key(id)) {
            self.parts.remove(txn, &id.to_string());
            self.stories.remove(txn, &id.to_string());
            self.seen.remove(&format!("part:{id}"));
        }
    }

    fn write_comments(&mut self, txn: &mut TransactionMut, old: &Document, new: &Document) {
        let comments = self.comments.clone();
        for (id, c) in &new.comments {
            if old.comments.get(id) == Some(c) && self.seen.contains_key(&format!("comment:{id}")) {
                continue;
            }
            if let Ok(json) = serde_json::to_string(c) {
                self.put(txn, &comments, "comment", &id.to_string(), json);
            }
        }
        for id in old.comments.keys().filter(|id| !new.comments.contains_key(id)) {
            self.comments.remove(txn, &id.to_string());
            self.seen.remove(&format!("comment:{id}"));
        }
    }

    fn write_media(&mut self, txn: &mut TransactionMut, old: &Document, new: &Document) {
        for (k, bytes) in &new.media {
            if old.media.get(k).is_some_and(|o| Arc::ptr_eq(o, bytes) || o == bytes) {
                continue;
            }
            self.media.insert(txn, k.as_str(), Any::Buffer(Arc::from(bytes.as_slice())));
        }
        for k in old.media.keys().filter(|k| !new.media.contains_key(*k)) {
            self.media.remove(txn, k.as_str());
        }
    }

    /// Merge another participant's update into `current` (local edits not sent yet are written
    /// first and returned, to send).
    pub fn remote(&mut self, update: &[u8], current: &Arc<Document>) -> Result<(Arc<Document>, Option<Vec<u8>>)> {
        let pending = self.local(current);
        {
            let update = Update::decode_v1(update).map_err(|e| CollabError::Update(e.to_string()))?;
            let mut txn = self.doc.transact_mut();
            txn.apply_update(update).map_err(|e| CollabError::Update(e.to_string()))?;
        }
        let base = self.synced.clone().unwrap_or_else(|| current.clone());
        let d = Arc::new(self.materialize(&base)?);
        self.synced = Some(d.clone());
        Ok((d, pending))
    }

    /// Undo or redo this participant's last step.
    pub fn undo(&mut self, current: &Arc<Document>, redo: bool) -> Result<Option<(Arc<Document>, Vec<u8>)>> {
        let mut out = Vec::new();
        if let Some(u) = self.local(current) {
            out.push(u);
        }
        self.step();
        let before = self.doc.transact().state_vector();
        let done = if redo { self.undo.redo_blocking() } else { self.undo.undo_blocking() };
        if !done {
            return Ok(None);
        }
        out.push(self.doc.transact().encode_diff_v1(&before));
        let base = self.synced.clone().unwrap_or_else(|| current.clone());
        let d = Arc::new(self.materialize(&base)?);
        self.synced = Some(d.clone());
        Ok(Some((d, merge(&out))))
    }

    fn materialize(&mut self, base: &Document) -> Result<Document> {
        let doc = self.doc.clone();
        let txn = doc.transact();
        let mut touched: HashSet<String> = HashSet::new();
        let mut fields = serde_json::Map::new();
        let mut meta_changed = false;
        for (k, v) in self.meta.iter(&txn) {
            if let Out::Any(Any::String(s)) = v {
                let key = format!("meta:{k}");
                if !self.seen.get(&key).is_some_and(|old| Arc::ptr_eq(old, &s) || *old == s) {
                    meta_changed = true;
                }
                if let Ok(value) = serde_json::from_str::<Value>(&s) {
                    fields.insert(k.to_string(), value);
                }
                touched.insert(key.clone());
                self.seen.insert(key, s);
            }
        }
        let mut d = if meta_changed || self.synced.as_ref().is_none_or(|s| s.body.is_empty()) {
            fields.insert("body".into(), Value::Array(vec![]));
            serde_json::from_value::<Document>(Value::Object(fields)).map_err(|e| CollabError::Shape(e.to_string()))?
        } else {
            Document { body: Vec::new(), parts: BTreeMap::new(), comments: BTreeMap::new(), media: BTreeMap::new(), ..base.clone() }
        };
        d.parts = BTreeMap::new();
        d.comments = BTreeMap::new();
        d.media = BTreeMap::new();

        // The stories.
        if let Some(Out::YText(t)) = self.stories.get(&txn, "body") {
            d.body = story::read(&txn, &t, &base.body, &self.cells, 0);
        }
        for (k, v) in self.parts.iter(&txn) {
            let Out::Any(Any::String(s)) = v else { continue };
            let Ok(id) = k.parse::<u32>() else { continue };
            let Ok(mut part) = serde_json::from_str::<Part>(&s) else { continue };
            let old = base.parts.get(&id).map(|p| p.blocks.clone()).unwrap_or_default();
            if let Some(Out::YText(t)) = self.stories.get(&txn, k) {
                part.blocks = story::read(&txn, &t, &old, &self.cells, 0);
            }
            touched.insert(format!("part:{k}"));
            self.seen.insert(format!("part:{k}"), s);
            d.parts.insert(id, part);
        }
        for (k, v) in self.comments.iter(&txn) {
            let Out::Any(Any::String(s)) = v else { continue };
            let (Ok(id), Ok(c)) = (k.parse::<u32>(), serde_json::from_str(&s)) else { continue };
            touched.insert(format!("comment:{k}"));
            self.seen.insert(format!("comment:{k}"), s);
            d.comments.insert(id, c);
        }
        for (k, v) in self.media.iter(&txn) {
            if let Out::Any(Any::Buffer(b)) = v {
                let bytes = match base.media.get(k) {
                    Some(old) if old.as_slice() == &*b => old.clone(),
                    _ => Arc::new(b.to_vec()),
                };
                d.media.insert(k.to_string(), bytes);
            }
        }
        self.seen.retain(|k, _| touched.contains(k));
        Ok(d)
    }

    /// Anchors for document positions, in a story's paragraphs or in table cells (`None`: the
    /// position cannot follow the text and stays as it is).
    pub fn anchors(&self, d: &Document, positions: &[&Pos]) -> Vec<Option<Anchor>> {
        let txn = self.doc.transact();
        positions
            .iter()
            .map(|p| {
                let Some(Out::YText(t)) = self.stories.get(&txn, &story_key(p.story)) else { return None };
                let blocks = blocks_of(d, p.story)?;
                let (text, blocks, i, cell) = story::locate(&txn, t, blocks, &p.path.0, &self.cells)?;
                let index = blocks.get(..i)?.iter().map(|b| story::block_len(b)).sum::<usize>() + p.off;
                let sticky = story::anchor(&txn, &text, u32::try_from(index).ok()?)?;
                Some(Anchor { story: p.story, cell, sticky })
            })
            .collect()
    }

    /// Where the anchors are now in `d` (`None`: gone, keep the position as it was).
    pub fn positions(&self, d: &Document, anchors: &[Option<Anchor>]) -> Vec<Option<Pos>> {
        let txn = self.doc.transact();
        anchors
            .iter()
            .map(|a| {
                let a = a.as_ref()?;
                let mut index = story::resolve(&txn, &a.sticky)? as usize;
                let story_blocks = blocks_of(d, a.story)?;
                let (prefix, blocks) = match &a.cell {
                    None => (Vec::new(), story_blocks),
                    Some(id) => {
                        let Some(Out::YText(t)) = self.stories.get(&txn, &story_key(a.story)) else { return None };
                        story::find_cell(&txn, &t, story_blocks, id, &self.cells, 0)?
                    }
                };
                let at = |i: usize, off: usize| {
                    let mut path = prefix.clone();
                    path.push(i as u32);
                    Pos::new(a.story, wordcraft_doc::Path(path), off)
                };
                for (i, b) in blocks.iter().enumerate() {
                    let len = story::block_len(b);
                    if index < len {
                        let off = match b.as_ref() {
                            wordcraft_doc::Block::Para(p) => (0..=index.min(p.text.len())).rev().find(|o| p.text.is_char_boundary(*o)).unwrap_or(0),
                            wordcraft_doc::Block::Table(_) => 0,
                        };
                        return Some(at(i, off));
                    }
                    index -= len;
                }
                let last = blocks.len().checked_sub(1)?;
                let off = blocks.get(last)?.as_para().map(|p| p.text.len()).unwrap_or(0);
                Some(at(last, off))
            })
            .collect()
    }
}

/// Several updates as one.
pub fn merge(updates: &[Vec<u8>]) -> Vec<u8> {
    match updates {
        [one] => one.clone(),
        _ => yrs::merge_updates_v1(updates).unwrap_or_else(|_| updates.concat()),
    }
}

#[cfg(test)]
mod tests;
