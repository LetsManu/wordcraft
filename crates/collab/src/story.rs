//! A story (the body, a header, a footnote, a comment…) as one shared rich text, so several
//! people can type in the same document, even in the same paragraph, at once.
//!
//! Each character of a paragraph carries its run's formatting (`r`, the character properties as
//! JSON); an inline object's U+FFFC carries the object (`o`). Every paragraph ends with a U+2029
//! (paragraph separator; `\n` is a line break inside a paragraph here) whose `p` attribute holds the
//! paragraph's own settings (style, alignment, numbering, mark, section).
//!
//! A table is one character, U+E000, followed by its own U+2029. Its `t` attribute holds the
//! table's structure (rows, cells and their settings, without the cells' text) and an id per cell;
//! each cell's text is a shared text of its own in the `cells` map, written the same way. So people
//! type in the same table, even in the same cell, at once; a change of the structure itself (a new
//! row, a wider column) replaces the structure as one piece (the last one wins).
//!
//! Offsets are UTF-8 bytes, like the engine's [`wordcraft_doc::Pos::off`], so a position in a
//! paragraph is the bytes of the blocks before it in its story or cell plus its offset.

use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use wordcraft_doc::para::OBJ;
use wordcraft_doc::table::{Cell, Row, Table};
use wordcraft_doc::{Block, Blocks, CharProps, InlineObject, Paragraph, Run, para_block};
use yrs::types::Attrs;
use yrs::types::text::{Diff, YChange};
use yrs::{Any, Assoc, IndexedSequence, Map, MapRef, Out, ReadTxn, StickyIndex, Text, TextPrelim, TextRef, TransactionMut};

const R: &str = "r";
const O: &str = "o";
const P: &str = "p";
const T: &str = "t";
/// Stands for a table in the shared text.
const TABLE: char = '\u{E000}';
/// Ends a paragraph (or a table) in the shared text.
const PARA: char = '\u{2029}';
/// Tables nested deeper than this are shared as one piece.
const MAX_DEPTH: usize = 16;

/// A table in the shared text: its structure (cells without their text) and the id of each
/// cell's shared text.
#[derive(Serialize, Deserialize)]
struct Skeleton {
    table: Table,
    ids: Vec<Vec<String>>,
}

/// Where cell texts live, and how this participant names new ones.
pub(crate) struct Ctx<'a> {
    pub cells: &'a MapRef,
    pub client: u64,
    pub next: &'a mut u64,
}

impl Ctx<'_> {
    fn fresh(&mut self) -> String {
        *self.next = self.next.saturating_add(1);
        format!("{:x}.{}", self.client, self.next)
    }
}

/// A caret: its path relative to the blocks being written, and its offset.
pub(crate) type Caret<'a> = Option<(&'a [u32], usize)>;

#[derive(Clone, Debug)]
struct Unit {
    ch: char,
    r: Option<Arc<str>>,
    o: Option<Arc<str>>,
    p: Option<Arc<str>>,
    t: Option<Arc<str>>,
    /// For a table: its block index in the blocks being written.
    table: Option<usize>,
}

impl Unit {
    fn plain(ch: char) -> Unit {
        Unit { ch, r: None, o: None, p: None, t: None, table: None }
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

fn block_units(block: &Block, index: usize, out: &mut Vec<Unit>) {
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
        Block::Table(_) => {
            out.push(Unit { table: Some(index), ..Unit::plain(TABLE) });
            out.push(Unit::plain(PARA));
        }
    }
}

fn units(blocks: &[Arc<Block>], first: usize) -> Vec<Unit> {
    let mut out = Vec::new();
    for (i, b) in blocks.iter().enumerate() {
        block_units(b, first + i, &mut out);
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

/// The caret as a byte offset in the shared text of `blocks`, when it is in one of their own
/// paragraphs.
fn caret_offset(blocks: &Blocks, caret: Caret) -> Option<usize> {
    let (path, off) = caret?;
    let [i] = path else { return None };
    Some(blocks.get(..*i as usize)?.iter().map(|b| block_len(b)).sum::<usize>() + off)
}

/// Bring the shared text from `old` to `new`. Unchanged blocks at both ends are skipped (they
/// are the same `Arc`s); inside the rest only the characters that differ are replaced, and
/// formatting that differs is applied as formatting; tables are brought up to date cell by cell.
/// `caret` decides where a change goes when the text alone cannot tell (a space typed next to a
/// space).
pub(crate) fn write(txn: &mut TransactionMut, text: &TextRef, old: &Blocks, new: &Blocks, caret: Caret, cx: &mut Ctx, depth: usize) {
    let same = |a: &Arc<Block>, b: &Arc<Block>| Arc::ptr_eq(a, b) || a == b;
    let head = old.iter().zip(new).take_while(|(a, b)| same(a, b)).count();
    let max_tail = old.len().min(new.len()) - head;
    let tail = old.iter().rev().zip(new.iter().rev()).take(max_tail).take_while(|(a, b)| same(a, b)).count();
    if head == old.len() && head == new.len() {
        return;
    }
    let base: usize = old.iter().take(head).map(|b| block_len(b)).sum();
    let a = units(&old[head..old.len() - tail], head);
    let b = units(&new[head..new.len() - tail], head);
    // The structure of the tables as they are now, read before anything changes.
    let skeletons = if a.iter().any(|u| u.table.is_some()) { skeletons(txn, text) } else { HashMap::new() };
    // The caret as a unit index into `b`.
    let caret_unit: Option<usize> = caret_offset(new, caret).and_then(|c| c.checked_sub(base)).and_then(|mut rest| {
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
    if let Some(caret) = caret_unit {
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
        if let Some(index) = middle[i].table {
            // A new table: its cells get texts of their own.
            let t = new.get(index).and_then(|b| b.as_table()).and_then(|t| new_table(txn, t, caret_tail(caret, index), cx, depth));
            let unit = Unit { t: t.map(Arc::from), ..Unit::plain(TABLE) };
            text.insert_with_attributes(txn, at, &TABLE.to_string(), unit.attrs());
            at += TABLE.len_utf8() as u32;
            i += 1;
            continue;
        }
        let mut j = i + 1;
        while j < middle.len() && middle[j].table.is_none() && middle[j].same_format(&middle[i]) {
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

    // Tables that stayed: bring their cells and structure up to date.
    let mut pairs: Vec<(usize, usize, u32)> = Vec::new();
    let mut pos = base as u32;
    for (x, y) in a[..pre].iter().zip(&b[..pre]) {
        if let (Some(i), Some(j)) = (x.table, y.table) {
            pairs.push((i, j, pos));
        }
        pos += y.ch.len_utf8() as u32;
    }
    let mut pos = at;
    for (x, y) in a[a.len() - suf..].iter().zip(&b[b.len() - suf..]) {
        if let (Some(i), Some(j)) = (x.table, y.table) {
            pairs.push((i, j, pos));
        }
        pos += y.ch.len_utf8() as u32;
    }
    for (i, j, pos) in pairs {
        let (Some(ot), Some(nt)) = (old.get(i).and_then(|b| b.as_table()), new.get(j).and_then(|b| b.as_table())) else { continue };
        if ot == nt {
            continue;
        }
        let tc = caret_tail(caret, j);
        let changed = match skeletons.get(&i) {
            Some((sk, raw)) if depth < MAX_DEPTH => sync_table(txn, ot, nt, sk, raw, tc, cx, depth),
            _ => new_table(txn, nt, tc, cx, depth),
        };
        if let Some(t) = changed {
            let attrs = Attrs::from([(Arc::from(T), Any::String(Arc::from(t)))]);
            text.format(txn, pos, TABLE.len_utf8() as u32, attrs);
        }
    }
}

/// The caret relative to the table at block `index` (row, cell, and the rest), if it is in it.
fn caret_tail(caret: Caret, index: usize) -> Caret {
    let (path, off) = caret?;
    match path {
        [b, rest @ ..] if *b as usize == index && rest.len() >= 3 => Some((rest, off)),
        _ => None,
    }
}

fn cell_text(txn: &mut TransactionMut, cx: &Ctx, id: &str) -> (TextRef, bool) {
    match cx.cells.get(txn, id) {
        Some(Out::YText(t)) => (t, false),
        _ => (cx.cells.insert(txn, id, TextPrelim::new("")), true),
    }
}

fn skeleton_json(t: &Table, ids: Vec<Vec<String>>) -> Option<String> {
    let rows =
        t.rows.iter().map(|r| Row { cells: r.cells.iter().map(|c| Cell { blocks: Vec::new(), ..c.clone() }).collect(), ..r.clone() }).collect();
    let table = Table { rows, ..t.clone() };
    serde_json::to_string(&Skeleton { table, ids }).ok()
}

/// A table new to the shared text: every cell gets a text of its own. Too deep a table is
/// shared as one piece.
fn new_table(txn: &mut TransactionMut, t: &Table, caret: Caret, cx: &mut Ctx, depth: usize) -> Option<String> {
    if depth >= MAX_DEPTH {
        return serde_json::to_string(t).ok();
    }
    let empty = Vec::new();
    let mut ids = Vec::with_capacity(t.rows.len());
    for (r, row) in t.rows.iter().enumerate() {
        let mut rid = Vec::with_capacity(row.cells.len());
        for (c, cell) in row.cells.iter().enumerate() {
            let id = cx.fresh();
            let (text, _) = cell_text(txn, cx, &id);
            write(txn, &text, &empty, &cell.blocks, caret_in_cell(caret, r, c), cx, depth + 1);
            rid.push(id);
        }
        ids.push(rid);
    }
    skeleton_json(t, ids)
}

fn caret_in_cell(caret: Caret, row: usize, cell: usize) -> Caret {
    let (path, off) = caret?;
    match path {
        [r, c, rest @ ..] if *r as usize == row && *c as usize == cell && !rest.is_empty() => Some((rest, off)),
        _ => None,
    }
}

/// Which old item each new one continues: equal ones at both ends, the rest in order.
fn align<T: PartialEq>(old: &[T], new: &[T]) -> Vec<Option<usize>> {
    let pre = old.iter().zip(new).take_while(|(a, b)| a == b).count();
    let max = old.len().min(new.len()) - pre;
    let suf = old.iter().rev().zip(new.iter().rev()).take(max).take_while(|(a, b)| a == b).count();
    let mut out = vec![None; new.len()];
    for (i, slot) in out.iter_mut().enumerate() {
        if i < pre {
            *slot = Some(i);
        } else if i >= new.len() - suf {
            *slot = Some(old.len() - (new.len() - i));
        } else if i - pre < old.len() - pre - suf {
            *slot = Some(i);
        }
    }
    out
}

/// Bring a table that is in the shared text already up to date: the cells that stayed keep their
/// texts and get only what changed in them. Returns the new structure when it changed.
#[allow(clippy::too_many_arguments)]
fn sync_table(
    txn: &mut TransactionMut,
    old: &Table,
    new: &Table,
    sk: &Skeleton,
    raw: &str,
    caret: Caret,
    cx: &mut Ctx,
    depth: usize,
) -> Option<String> {
    let empty = Vec::new();
    let rows = align(&old.rows, &new.rows);
    let mut ids = Vec::with_capacity(new.rows.len());
    for (r, row) in new.rows.iter().enumerate() {
        let old_row = rows.get(r).copied().flatten().and_then(|o| Some((o, old.rows.get(o)?)));
        let cells = match old_row {
            Some((_, orow)) => align(&orow.cells, &row.cells),
            None => vec![None; row.cells.len()],
        };
        let mut rid = Vec::with_capacity(row.cells.len());
        for (c, cell) in row.cells.iter().enumerate() {
            let prev = old_row.and_then(|(o, orow)| {
                let oc = cells.get(c).copied().flatten()?;
                Some((sk.ids.get(o)?.get(oc)?.clone(), orow.cells.get(oc)?))
            });
            let cc = caret_in_cell(caret, r, c);
            let id = match prev {
                Some((id, ocell)) => {
                    if ocell.blocks != cell.blocks {
                        let (text, created) = cell_text(txn, cx, &id);
                        write(txn, &text, if created { &empty } else { &ocell.blocks }, &cell.blocks, cc, cx, depth + 1);
                    }
                    id
                }
                None => {
                    let id = cx.fresh();
                    let (text, _) = cell_text(txn, cx, &id);
                    write(txn, &text, &empty, &cell.blocks, cc, cx, depth + 1);
                    id
                }
            };
            rid.push(id);
        }
        ids.push(rid);
    }
    let s = skeleton_json(new, ids)?;
    (s != raw).then_some(s)
}

fn attr(a: &Option<Box<Attrs>>, key: &str) -> Option<Arc<str>> {
    match a.as_ref()?.get(key)? {
        Any::String(s) => Some(s.clone()),
        _ => None,
    }
}

/// The structure of every table in the text, by block index.
fn skeletons<T: ReadTxn>(txn: &T, text: &TextRef) -> HashMap<usize, (Skeleton, Arc<str>)> {
    let mut out = HashMap::new();
    let mut block = 0usize;
    for chunk in text.diff(txn, YChange::identity) {
        let Out::Any(Any::String(s)) = &chunk.insert else { continue };
        let t = attr(&chunk.attributes, T);
        for ch in s.chars() {
            match ch {
                PARA => block += 1,
                TABLE => {
                    if let Some(raw) = &t
                        && let Ok(sk) = serde_json::from_str::<Skeleton>(raw)
                    {
                        out.insert(block, (sk, raw.clone()));
                    }
                }
                _ => {}
            }
        }
    }
    out
}

fn read_table<T: ReadTxn>(txn: &T, raw: &str, cells: &MapRef, depth: usize) -> Option<Table> {
    let Ok(sk) = serde_json::from_str::<Skeleton>(raw) else {
        // Shared as one piece.
        return serde_json::from_str(raw).ok();
    };
    let mut table = sk.table;
    for (r, row) in table.rows.iter_mut().enumerate() {
        for (c, cell) in row.cells.iter_mut().enumerate() {
            let text = sk.ids.get(r).and_then(|x| x.get(c)).and_then(|id| match cells.get(txn, id) {
                Some(Out::YText(t)) => Some(t),
                _ => None,
            });
            cell.blocks = match text {
                Some(t) if depth < MAX_DEPTH => read(txn, &t, &[], cells, depth + 1),
                _ => vec![para_block(Paragraph::new())],
            };
        }
    }
    Some(table)
}

/// The blocks the shared text describes; blocks equal to one in `old` (at the start or the end)
/// keep its `Arc`, so layout caches stay valid.
pub(crate) fn read<T: ReadTxn>(txn: &T, text: &TextRef, old: &[Arc<Block>], cells: &MapRef, depth: usize) -> Blocks {
    let chunks: Vec<Diff<YChange>> = text.diff(txn, YChange::identity);
    let mut blocks: Vec<Block> = Vec::new();
    let mut cur = Building::default();
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
                    if let Some(table) = t.as_deref().and_then(|j| read_table(txn, j, cells, depth)) {
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
    table: Option<Table>,
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

/// The shared text holding the paragraph at `path` (a path into a story whose text is `text`
/// and blocks `blocks`): that text, its blocks, the block index in them, and the cell's id
/// (`None` for the story itself).
pub(crate) fn locate<'d, T: ReadTxn>(
    txn: &T,
    text: TextRef,
    blocks: &'d Blocks,
    path: &[u32],
    cells: &MapRef,
) -> Option<(TextRef, &'d Blocks, usize, Option<String>)> {
    let (mut text, mut blocks, mut rest, mut id) = (text, blocks, path, None);
    while let [b, r, c, more @ ..] = rest {
        if more.is_empty() {
            break;
        }
        let skels = skeletons(txn, &text);
        let cell = skels.get(&(*b as usize))?.0.ids.get(*r as usize)?.get(*c as usize)?.clone();
        text = match cells.get(txn, &cell) {
            Some(Out::YText(t)) => t,
            _ => return None,
        };
        blocks = &blocks.get(*b as usize)?.as_table()?.rows.get(*r as usize)?.cells.get(*c as usize)?.blocks;
        rest = more;
        id = Some(cell);
    }
    let [i] = rest else { return None };
    Some((text, blocks, *i as usize, id))
}

/// Where the cell with text `id` is now: the path prefix to its blocks, and the blocks.
pub(crate) fn find_cell<'d, T: ReadTxn>(
    txn: &T,
    text: &TextRef,
    blocks: &'d Blocks,
    id: &str,
    cells: &MapRef,
    depth: usize,
) -> Option<(Vec<u32>, &'d Blocks)> {
    if depth >= MAX_DEPTH {
        return None;
    }
    for (b, (sk, _)) in skeletons(txn, text) {
        let Some(table) = blocks.get(b).and_then(|x| x.as_table()) else { continue };
        for (r, row) in sk.ids.iter().enumerate() {
            for (c, cell) in row.iter().enumerate() {
                let Some(inner) = table.rows.get(r).and_then(|x| x.cells.get(c)) else { continue };
                let prefix = vec![b as u32, r as u32, c as u32];
                if cell == id {
                    return Some((prefix, &inner.blocks));
                }
                if let Some(Out::YText(t)) = cells.get(txn, cell)
                    && let Some((mut deeper, found)) = find_cell(txn, &t, &inner.blocks, id, cells, depth + 1)
                {
                    let mut p = prefix;
                    p.append(&mut deeper);
                    return Some((p, found));
                }
            }
        }
    }
    None
}

/// A position that moves with the text when others type before it (tied to the character before
/// it, so it stays right after what this person typed).
pub(crate) fn anchor<T: ReadTxn>(txn: &T, text: &TextRef, index: u32) -> Option<StickyIndex> {
    text.sticky_index(txn, index, Assoc::Before)
}

pub(crate) fn resolve<T: ReadTxn>(txn: &T, sticky: &StickyIndex) -> Option<u32> {
    sticky.get_offset(txn).map(|o| o.index)
}
