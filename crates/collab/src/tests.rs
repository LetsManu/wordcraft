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

fn table_mut(d: &mut Document) -> &mut wordcraft_doc::table::Table {
    match d.body.get_mut(2).map(Arc::make_mut) {
        Some(Block::Table(t)) => t,
        _ => panic!("no table"),
    }
}

fn type_in_cell(d: &mut Document, row: usize, cell: usize, off: usize, s: &str) {
    let t = table_mut(d);
    if let Some(Block::Para(p)) = t.rows[row].cells[cell].blocks.get_mut(0).map(Arc::make_mut) {
        p.text.insert_str(off, s);
        match p.runs.last_mut() {
            Some(r) => r.len += s.len(),
            None => p.runs.push(Run { len: s.len(), props: CharProps::default() }),
        }
    }
}

fn cell_text(d: &Document, row: usize, cell: usize) -> String {
    let t = d.body[2].as_table().unwrap();
    t.rows[row].cells[cell].blocks.iter().filter_map(|b| b.as_para()).map(|p| p.text.clone()).collect::<Vec<_>>().join("|")
}

#[test]
fn typing_in_the_same_table_cell_keeps_both() {
    let mut peers = session(2);
    let a = peers[0].edit(|d| type_in_cell(d, 0, 0, 0, "Hello"));
    peers[1].receive(&a);
    let a = peers[0].edit(|d| type_in_cell(d, 0, 0, 0, ">> "));
    let b = peers[1].edit(|d| type_in_cell(d, 0, 0, 5, " world"));
    peers[0].receive(&b);
    peers[1].receive(&a);
    assert_eq!(peers[0].d.body, peers[1].d.body);
    assert_eq!(cell_text(&peers[0].d, 0, 0), ">> Hello world");
}

#[test]
fn a_new_row_and_typing_in_a_cell_merge() {
    let mut peers = session(2);
    let a = peers[0].edit(|d| type_in_cell(d, 0, 0, 0, "Item"));
    peers[1].receive(&a);
    let a = peers[0].edit(|d| {
        let t = table_mut(d);
        let mut row = t.rows[0].clone();
        row.cells = vec![wordcraft_doc::table::Cell::empty(), wordcraft_doc::table::Cell::empty()];
        t.rows.insert(0, row);
    });
    let b = peers[1].edit(|d| type_in_cell(d, 1, 1, 0, "Total"));
    peers[0].receive(&b);
    peers[1].receive(&a);
    assert_eq!(peers[0].d.body, peers[1].d.body);
    assert_eq!(peers[0].d.body[2].as_table().unwrap().rows.len(), 3);
    assert_eq!(cell_text(&peers[0].d, 2, 1), "Total");
    assert_eq!(cell_text(&peers[0].d, 0, 1), "");
}

#[test]
fn cell_formatting_and_new_tables_arrive() {
    let mut peers = session(2);
    let a = peers[0].edit(|d| {
        type_in_cell(d, 0, 1, 0, "bold");
        let t = table_mut(d);
        if let Some(Block::Para(p)) = t.rows[0].cells[1].blocks.get_mut(0).map(Arc::make_mut) {
            p.runs = vec![Run { len: 4, props: CharProps { bold: Some(true), ..Default::default() } }];
        }
        let mut inner = wordcraft_doc::table::Table::new(1, 2, 100.0);
        if let Some(Block::Para(p)) = inner.rows[0].cells[0].blocks.get_mut(0).map(Arc::make_mut) {
            p.text = "nested".into();
            p.runs = vec![Run { len: 6, props: CharProps::default() }];
        }
        t.rows[1].cells[0].blocks.insert(0, Arc::new(Block::Table(inner)));
        d.body.push(Arc::new(Block::Table(wordcraft_doc::table::Table::new(3, 3, 300.0))));
    });
    peers[1].receive(&a);
    assert_eq!(peers[0].d.body, peers[1].d.body);
    // Typing in the nested table merges too.
    let a = peers[0].edit(|d| {
        let t = table_mut(d);
        if let Some(Block::Table(inner)) = t.rows[1].cells[0].blocks.get_mut(0).map(Arc::make_mut)
            && let Some(Block::Para(p)) = inner.rows[0].cells[0].blocks.get_mut(0).map(Arc::make_mut)
        {
            p.text.push_str(" A");
            p.runs[0].len += 2;
        }
    });
    let b = peers[1].edit(|d| {
        let t = table_mut(d);
        if let Some(Block::Table(inner)) = t.rows[1].cells[0].blocks.get_mut(0).map(Arc::make_mut)
            && let Some(Block::Para(p)) = inner.rows[0].cells[0].blocks.get_mut(0).map(Arc::make_mut)
        {
            p.text.insert_str(0, "B ");
            p.runs[0].len += 2;
        }
    });
    peers[0].receive(&b);
    peers[1].receive(&a);
    assert_eq!(peers[0].d.body, peers[1].d.body);
    let inner = peers[0].d.body[2].as_table().unwrap().rows[1].cells[0].blocks[0].as_table().unwrap();
    assert_eq!(inner.rows[0].cells[0].blocks[0].as_para().unwrap().text, "B nested A");
}

#[test]
fn undo_in_a_cell_takes_back_only_my_step() {
    let mut peers = session(2);
    let a = peers[0].edit(|d| type_in_cell(d, 0, 0, 0, "mine"));
    peers[0].c.step();
    let b = peers[1].edit(|d| type_in_cell(d, 0, 0, 0, "theirs "));
    peers[0].receive(&b);
    peers[1].receive(&a);
    let cur = peers[0].d.clone();
    let (d, u) = peers[0].c.undo(&cur, false).unwrap().unwrap();
    peers[0].d = d;
    peers[1].receive(&u);
    assert_eq!(peers[0].d.body, peers[1].d.body);
    assert_eq!(cell_text(&peers[0].d, 0, 0), "theirs ");
}

#[test]
fn a_caret_in_a_cell_follows_text_and_new_rows() {
    let mut peers = session(2);
    let a = peers[0].edit(|d| type_in_cell(d, 1, 0, 0, "abc"));
    peers[1].receive(&a);
    let caret = Pos::new(StoryRef::Body, Path(vec![2, 1, 0, 0]), 2);
    let anchors = peers[1].c.anchors(&peers[1].d, &[&caret]);
    assert!(anchors[0].is_some());
    let a1 = peers[0].edit(|d| type_in_cell(d, 1, 0, 0, "xy"));
    peers[1].receive(&a1);
    let a = peers[0].edit(|d| {
        let t = table_mut(d);
        let row = t.rows[0].clone();
        t.rows.insert(0, row);
    });
    peers[1].receive(&a);
    let d = peers[1].d.clone();
    assert_eq!(peers[1].c.positions(&d, &anchors), vec![Some(Pos::new(StoryRef::Body, Path(vec![2, 2, 0, 0]), 4))]);
}

#[test]
fn spaces_typed_next_to_spaces_land_where_typed() {
    let mut peers = session(2);
    let a = peers[0].edit(|d| type_at(d, 1, 6, " "));
    peers[1].receive(&a);
    assert_eq!(texts(&peers[1].d)[1], "Second  one.");
    // Both type a space at the double space at once, then a letter after it.
    peers[0].c.set_caret(Some(Pos::new(StoryRef::Body, Path::top(1), 8)));
    let a = peers[0].edit(|d| type_at(d, 1, 7, " A"));
    peers[1].c.set_caret(Some(Pos::new(StoryRef::Body, Path::top(1), 7)));
    let b = peers[1].edit(|d| type_at(d, 1, 6, " B"));
    peers[0].receive(&b);
    peers[1].receive(&a);
    assert_eq!(peers[0].d.body, peers[1].d.body);
    // Each lands exactly where it was typed: B before the first space, A between the two.
    assert_eq!(texts(&peers[0].d)[1], "Second B  A one.");
    // Backspace over one of two spaces removes the one before the caret.
    peers[0].c.set_caret(Some(Pos::new(StoryRef::Body, Path::top(1), 6)));
    let a = peers[0].edit(|d| {
        if let Some(Block::Para(p)) = d.body.get_mut(1).map(Arc::make_mut) {
            p.text.remove(6);
            p.runs[0].len -= 1;
        }
    });
    peers[1].receive(&a);
    assert_eq!(peers[0].d.body, peers[1].d.body);
}

#[test]
fn ids_from_others_in_cells_never_clash() {
    let mut peers = session(3);
    let a = peers[0].edit(|d| d.body.push(Arc::new(Block::Table(wordcraft_doc::table::Table::new(1, 1, 50.0)))));
    let b = peers[1].edit(|d| d.body.push(Arc::new(Block::Table(wordcraft_doc::table::Table::new(1, 1, 50.0)))));
    for p in [0, 2] {
        peers[p].receive(&b);
    }
    for p in [1, 2] {
        peers[p].receive(&a);
    }
    assert_eq!(peers[0].d.body, peers[1].d.body);
    assert_eq!(peers[2].d.body, peers[1].d.body);
    assert_eq!(peers[0].d.body.iter().filter(|b| b.as_table().is_some()).count(), 3);
}
