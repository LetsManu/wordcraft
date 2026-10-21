//! Replicas edit one document and must end with the same content.

use std::sync::Arc;

use wordcraft_doc::{Block, CharProps, Document, Paragraph, Path, Pos, Run, StoryRef, para_block};

use super::*;

fn para(text: &str) -> Arc<Block> {
    para_block(Paragraph { text: text.into(), runs: vec![Run { len: text.len(), props: CharProps::default() }], ..Default::default() })
}

fn sample() -> Document {
    let mut d = Document::new();
    d.body = vec![para("First paragraph."), para("Second one."), para("Third.")];
    d.core.title = "Shared".into();
    let mut t = wordcraft_doc::table::Table::new(2, 2, 300.0);
    t.grid = vec![150.0, 150.0];
    d.body.insert(2, Arc::new(Block::Table(t)));
    d
}

fn texts(d: &Document) -> Vec<String> {
    d.body.iter().map(|b| b.as_para().map(|p| p.text.clone()).unwrap_or_else(|| "<table>".into())).collect()
}

struct Peer {
    c: Collab,
    d: Arc<Document>,
}

impl Peer {
    fn edit(&mut self, f: impl FnOnce(&mut Document)) -> Vec<u8> {
        let mut q = (*self.d).clone();
        f(&mut q);
        self.d = Arc::new(q);
        self.c.local(&self.d).unwrap_or_default()
    }
    fn receive(&mut self, u: &[u8]) {
        let (d, pending) = self.c.remote(u, &self.d).unwrap();
        assert!(pending.is_none());
        self.d = d;
    }
}

fn session(n: u64) -> Vec<Peer> {
    let mut first = Collab::new(1);
    let doc = sample();
    let seed = first.seed(&doc);
    let d = Arc::new(doc);
    first.synced = Some(d.clone());
    let mut peers = vec![Peer { c: first, d }];
    for i in 1..n {
        let mut c = Collab::new(i + 1);
        let d = Arc::new(c.join(std::slice::from_ref(&seed)).unwrap());
        peers.push(Peer { c, d });
    }
    peers
}

fn type_at(d: &mut Document, block: usize, off: usize, s: &str) {
    if let Some(Block::Para(p)) = d.body.get_mut(block).map(Arc::make_mut) {
        p.text.insert_str(off, s);
        if let Some(r) = p.runs.last_mut() {
            r.len += s.len();
        }
    }
}

#[test]
fn joining_gives_the_same_document() {
    let peers = session(2);
    assert_eq!(peers[0].d.body, peers[1].d.body);
    assert_eq!(peers[1].d.core.title, "Shared");
    assert_eq!(texts(&peers[1].d), vec!["First paragraph.", "Second one.", "<table>", "Third."]);
}

#[test]
fn typing_in_the_same_paragraph_keeps_both() {
    let mut peers = session(2);
    let a = peers[0].edit(|d| type_at(d, 0, 0, "A: "));
    let b = peers[1].edit(|d| type_at(d, 0, "First paragraph.".len(), " (B)"));
    peers[0].receive(&b);
    peers[1].receive(&a);
    assert_eq!(peers[0].d.body, peers[1].d.body);
    assert_eq!(texts(&peers[0].d)[0], "A: First paragraph. (B)");
}

#[test]
fn new_paragraphs_and_formatting_merge() {
    let mut peers = session(2);
    let a = peers[0].edit(|d| d.body.insert(1, para("Inserted by A")));
    let b = peers[1].edit(|d| {
        if let Some(Block::Para(p)) = d.body.get_mut(3).map(Arc::make_mut) {
            p.runs = vec![Run { len: p.text.len(), props: CharProps { bold: Some(true), ..Default::default() } }];
        }
    });
    peers[0].receive(&b);
    peers[1].receive(&a);
    assert_eq!(peers[0].d.body, peers[1].d.body);
    assert_eq!(texts(&peers[0].d), vec!["First paragraph.", "Inserted by A", "Second one.", "<table>", "Third."]);
    let third = peers[0].d.body[4].as_para().unwrap();
    assert_eq!(third.runs[0].props.bold, Some(true));
}

#[test]
fn unchanged_paragraphs_keep_their_arc() {
    let mut peers = session(2);
    let before = peers[1].d.body.clone();
    let a = peers[0].edit(|d| type_at(d, 1, 0, "x"));
    peers[1].receive(&a);
    assert!(Arc::ptr_eq(&before[0], &peers[1].d.body[0]));
    assert!(!Arc::ptr_eq(&before[1], &peers[1].d.body[1]));
    assert!(Arc::ptr_eq(&before[3], &peers[1].d.body[3]));
}

#[test]
fn undo_takes_back_only_my_step() {
    let mut peers = session(2);
    let a = peers[0].edit(|d| type_at(d, 0, 0, "mine "));
    peers[0].c.step();
    let b = peers[1].edit(|d| type_at(d, 1, 0, "theirs "));
    peers[0].receive(&b);
    peers[1].receive(&a);
    let cur = peers[0].d.clone();
    let (d, u) = peers[0].c.undo(&cur, false).unwrap().unwrap();
    peers[0].d = d;
    peers[1].receive(&u);
    assert_eq!(peers[0].d.body, peers[1].d.body);
    assert_eq!(texts(&peers[0].d)[0], "First paragraph.");
    assert_eq!(texts(&peers[0].d)[1], "theirs Second one.");
}

#[test]
fn the_caret_follows_text_typed_before_it() {
    let mut peers = session(2);
    let caret = Pos::new(StoryRef::Body, Path::top(0), 5);
    let anchors = peers[1].c.anchors(&peers[1].d, &[&caret]);
    let a = peers[0].edit(|d| type_at(d, 0, 0, "abc"));
    peers[1].receive(&a);
    let d = peers[1].d.clone();
    assert_eq!(peers[1].c.positions(&d, &anchors), vec![Some(Pos::new(StoryRef::Body, Path::top(0), 8))]);
}

#[test]
fn garbage_is_an_error_not_a_crash() {
    let mut peers = session(1);
    let d = peers[0].d.clone();
    assert!(peers[0].c.remote(&[1, 2, 3], &d).is_err());
    assert!(Collab::new(5).join(&[vec![7, 7]]).is_err());
}

#[test]
fn line_breaks_inside_a_paragraph_survive() {
    let mut peers = session(2);
    let a = peers[0].edit(|d| type_at(d, 0, 5, "\nnext line"));
    peers[1].receive(&a);
    assert_eq!(peers[0].d.body, peers[1].d.body);
    assert_eq!(texts(&peers[1].d)[0], "First\nnext line paragraph.");
    assert_eq!(texts(&peers[1].d).len(), 4);
}
