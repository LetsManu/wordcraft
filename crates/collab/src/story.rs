//! A story (the body, a header, a footnote, a comment…) as one shared rich text, so several
//! people can type in the same document, even in the same paragraph, at once.
//!
//! Each character of a paragraph carries its run's formatting (`r`, the character properties as
//! JSON); an inline object's U+FFFC carries the object (`o`). Every paragraph ends with a U+2029
//! (paragraph separator; `\n` is a line break inside a paragraph here) whose `p` attribute holds the paragraph's own settings (style, alignment, numbering, mark,
//! section). A table is one character, U+E000, carrying the whole table (`t`), followed by its
//! own U+2029: a table changes as one piece (the last change wins), its text is not shared
//! character by character yet.
//!
//! Offsets are UTF-8 bytes, like the engine's [`wordcraft_doc::Pos::off`], so a position in a
//! top-level paragraph is the bytes of the blocks before it plus its offset.

use std::sync::Arc;

use wordcraft_doc::para::OBJ;
use wordcraft_doc::{Block, Blocks, CharProps, InlineObject, Paragraph, Run};
use yrs::types::Attrs;
use yrs::types::text::{Diff, YChange};
use yrs::{Any, Assoc, IndexedSequence, Out, ReadTxn, StickyIndex, Text, TextRef, TransactionMut};

const R: &str = "r";
const O: &str = "o";
const P: &str = "p";
const T: &str = "t";
/// Stands for a table in the shared text.
const TABLE: char = '\u{E000}';
/// Ends a paragraph (or a table) in the shared text.
const PARA: char = '\u{2029}';

#[derive(Clone, Debug, PartialEq)]
struct Unit {
    ch: char,
    r: Option<Arc<str>>,
    o: Option<Arc<str>>,
    p: Option<Arc<str>>,
    t: Option<Arc<str>>,
}

impl Unit {
    fn plain(ch: char) -> Unit {
        Unit { ch, r: None, o: None, p: None, t: None }
    }
    fn attrs(&self) -> Attrs {
        let v = |s: &Option<Arc<str>>| s.clone().map(Any::String).unwrap_or(Any::Null);
        Attrs::from([(Arc::from(R), v(&self.r)), (Arc::from(O), v(&self.o)), (Arc::from(P), v(&self.p)), (Arc::from(T), v(&self.t))])
    }
    fn same_format(&self, other: &Unit) -> bool {
        self.r == other.r && self.o == other.o && self.p == other.p && self.t == other.t
    }
}

fn json<T: serde::Serialize>(v: &T) -> Option<Arc<str>> {
    serde_json::to_string(v).ok().map(Arc::from)
}

/// The paragraph's own settings: everything but its text, runs and objects.
fn para_json(p: &Paragraph) -> Option<Arc<str>> {
    let q = Paragraph { text: String::new(), runs: Vec::new(), objects: Vec::new(), ..p.clone() };
    json(&q)
}

fn block_units(block: &Block, out: &mut Vec<Unit>) {
    match block {
        Block::Para(p) => {
            // The run covering each byte of the text.
            let mut props: Vec<(usize, Option<Arc<str>>)> = Vec::new();
            let mut at = 0;
            for run in &p.runs {
                let r = (run.props != CharProps::default()).then(|| json(&run.props)).flatten();
                props.push((at + run.len, r));
                at += run.len;
            }
            let mut objects = p.objects.iter();
            for (i, ch) in p.text.char_indices() {
                let r = props.iter().find(|(end, _)| i < *end).and_then(|(_, r)| r.clone());
                let mut u = Unit { r, ..Unit::plain(ch) };
                if ch == PARA || ch == TABLE {
                    // Same length in UTF-8, so offsets stay as the engine counts them.
                    u.ch = '\u{2028}';
                }
                if ch == OBJ {
                    u.o = objects.next().and_then(json);
                }
                out.push(u);
            }
            out.push(Unit { p: para_json(p), ..Unit::plain(PARA) });
        }
        Block::Table(t) => {
            out.push(Unit { t: json(t), ..Unit::plain(TABLE) });
            out.push(Unit::plain(PARA));
        }
    }
}

fn units(blocks: &[Arc<Block>]) -> Vec<Unit> {
    let mut out = Vec::new();
    for b in blocks {
        block_units(b, &mut out);
    }
    out
}

fn bytes(units: &[Unit]) -> u32 {
    units.iter().map(|u| u.ch.len_utf8() as u32).sum()
}

/// Bytes a block takes in the shared text.
pub(crate) fn block_len(block: &Block) -> usize {
    match block {
        Block::Para(p) => p.text.len() + PARA.len_utf8(),
        Block::Table(_) => TABLE.len_utf8() + PARA.len_utf8(),
    }
}

/// Bring the shared text from `old` to `new`. Unchanged blocks at both ends are skipped (they
/// are the same `Arc`s); inside the rest only the characters that differ are replaced, and
/// formatting that differs is applied as formatting. `caret` (a byte offset into `new`'s shared
/// text) decides where a change goes when the text alone cannot tell (a space typed next to a
/// space).
pub(crate) fn write(txn: &mut TransactionMut, text: &TextRef, old: &Blocks, new: &Blocks, caret: Option<usize>) {
    let same = |a: &Arc<Block>, b: &Arc<Block>| Arc::ptr_eq(a, b) || a == b;
    let head = old.iter().zip(new).take_while(|(a, b)| same(a, b)).count();
    let max_tail = old.len().min(new.len()) - head;
    let tail = old.iter().rev().zip(new.iter().rev()).take(max_tail).take_while(|(a, b)| same(a, b)).count();
    if head == old.len() && head == new.len() {
        return;
    }
    let base: usize = old.iter().take(head).map(|b| block_len(b)).sum();
    let a = units(&old[head..old.len() - tail]);
    let b = units(&new[head..new.len() - tail]);
    // The caret as a unit index into `b`.
    let caret: Option<usize> = caret.and_then(|c| c.checked_sub(base)).and_then(|mut rest| {
        let mut i = 0usize;
        for u in &b {
            if rest == 0 {
                return Some(i);
            }
            rest = rest.checked_sub(u.ch.len_utf8())?;
            i += 1;
        }
        (rest == 0).then_some(i)
    });
    let mut pre = a.iter().zip(&b).take_while(|(x, y)| x.ch == y.ch).count();
    let max_suf = a.len().min(b.len()) - pre;
    let mut suf = a.iter().rev().zip(b.iter().rev()).take(max_suf).take_while(|(x, y)| x.ch == y.ch).count();
    if let Some(caret) = caret {
        let (short, long) = (a.len().min(b.len()), a.len().max(b.len()));
        if pre + suf > short {
            let lowest = short.saturating_sub(suf);
            let highest = pre.min(short);
            let wanted = if b.len() > a.len() { caret.saturating_sub(long - short) } else { caret };
            let start = wanted.clamp(lowest, highest.max(lowest));
            pre = start;
            suf = short - start;
        }
    }
    let start = base as u32 + bytes(&b[..pre]);
    let removed = bytes(&a[pre..a.len() - suf]);
    if removed > 0 {
        text.remove_range(txn, start, removed);
    }
    let mut at = start;
    let middle = &b[pre..b.len() - suf];
    let mut i = 0;
    while i < middle.len() {
        let mut j = i + 1;
        while j < middle.len() && middle[j].same_format(&middle[i]) {
            j += 1;
        }
        let chunk: String = middle[i..j].iter().map(|u| u.ch).collect();
        text.insert_with_attributes(txn, at, &chunk, middle[i].attrs());
        at += chunk.len() as u32;
        i = j;
    }
    let mut format = |units_old: &[Unit], units_new: &[Unit], mut pos: u32| {
        let mut i = 0;
        while i < units_new.len() {
            if units_old[i].same_format(&units_new[i]) {
                pos += units_new[i].ch.len_utf8() as u32;
                i += 1;
                continue;
            }
            let mut j = i + 1;
            while j < units_new.len() && !units_old[j].same_format(&units_new[j]) && units_new[j].same_format(&units_new[i]) {
                j += 1;
            }
            let len = bytes(&units_new[i..j]);
            text.format(txn, pos, len, units_new[i].attrs());
            pos += len;
            i = j;
        }
    };
    format(&a[..pre], &b[..pre], base as u32);
    format(&a[a.len() - suf..], &b[b.len() - suf..], at);
}

/// The blocks the shared text describes; blocks equal to one in `old` (at the start or the end)
/// keep its `Arc`, so layout caches stay valid.
pub(crate) fn read<T: ReadTxn>(txn: &T, text: &TextRef, old: &Blocks) -> Blocks {
    let chunks: Vec<Diff<YChange>> = text.diff(txn, YChange::identity);
    let mut blocks: Vec<Block> = Vec::new();
    let mut cur = Building::default();
    let attr = |a: &Option<Box<Attrs>>, key: &str| -> Option<Arc<str>> {
        match a.as_ref()?.get(key)? {
            Any::String(s) => Some(s.clone()),
            _ => None,
        }
    };
    for chunk in chunks {
        let Out::Any(Any::String(s)) = &chunk.insert else { continue };
        let r = attr(&chunk.attributes, R);
        let o = attr(&chunk.attributes, O);
        let p = attr(&chunk.attributes, P);
        let t = attr(&chunk.attributes, T);
        let props: CharProps = r.as_deref().and_then(|j| serde_json::from_str(j).ok()).unwrap_or_default();
        for ch in s.chars() {
            match ch {
                PARA => blocks.push(cur.finish(p.as_deref())),
                TABLE => {
                    if let Some(table) = t.as_deref().and_then(|j| serde_json::from_str(j).ok()) {
                        cur.table = Some(table);
                    }
                }
                OBJ => {
                    if let Some(obj) = o.as_deref().and_then(|j| serde_json::from_str::<InlineObject>(j).ok()) {
                        cur.push(ch, &props);
                        cur.objects.push(obj);
                    }
                }
                _ => cur.push(ch, &props),
            }
        }
    }
    if !cur.text.is_empty() || cur.table.is_some() {
        blocks.push(cur.finish(None));
    }
    if blocks.is_empty() {
        blocks.push(Block::Para(Paragraph::default()));
    }
    // Keep the old `Arc`s of blocks that did not change.
    let head = old.iter().zip(&blocks).take_while(|(a, b)| a.as_ref() == *b).count();
    let max_tail = old.len().min(blocks.len()) - head;
    let tail = old.iter().rev().zip(blocks.iter().rev()).take(max_tail).take_while(|(a, b)| a.as_ref() == *b).count();
    let n = blocks.len();
    blocks
        .into_iter()
        .enumerate()
        .map(|(i, b)| {
            if i < head {
                old.get(i).cloned().unwrap_or_else(|| Arc::new(b))
            } else if i >= n - tail {
                old.get(old.len() - (n - i)).cloned().unwrap_or_else(|| Arc::new(b))
            } else {
                Arc::new(b)
            }
        })
        .collect()
}

#[derive(Default)]
struct Building {
    text: String,
    runs: Vec<Run>,
    objects: Vec<InlineObject>,
    table: Option<wordcraft_doc::table::Table>,
}

impl Building {
    fn push(&mut self, ch: char, props: &CharProps) {
        self.text.push(ch);
        match self.runs.last_mut() {
            Some(last) if last.props == *props => last.len += ch.len_utf8(),
            _ => self.runs.push(Run { len: ch.len_utf8(), props: props.clone() }),
        }
    }
    fn finish(&mut self, para: Option<&str>) -> Block {
        let b = std::mem::take(self);
        if let Some(t) = b.table {
            return Block::Table(t);
        }
        let mut p: Paragraph = para.and_then(|j| serde_json::from_str(j).ok()).unwrap_or_default();
        p.text = b.text;
        p.runs = b.runs;
        p.objects = b.objects;
        Block::Para(p)
    }
}

/// A position that moves with the text when others type before it (tied to the character before
/// it, so it stays right after what this person typed).
pub(crate) fn anchor<T: ReadTxn>(txn: &T, text: &TextRef, index: u32) -> Option<StickyIndex> {
    text.sticky_index(txn, index, Assoc::Before)
}

pub(crate) fn resolve<T: ReadTxn>(txn: &T, sticky: &StickyIndex) -> Option<u32> {
    sticky.get_offset(txn).map(|o| o.index)
}
