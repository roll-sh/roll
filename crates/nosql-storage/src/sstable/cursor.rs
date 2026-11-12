//! Reading one SSTable: partitions in token order, and each partition's items through a
//! [`Clipper`].
//!
//! The cursor is a synchronous state machine over decoded blocks. When it needs a block it
//! doesn't have, it returns [`Step::Pending`] with the block range to fetch (plus read-ahead);
//! the driver fetches it (asynchronously on the read path, blocking inside compaction) and
//! [`SstCursor::supply`]s it. Merge logic therefore never awaits, and the same code serves
//! reads and compaction.
//!
//! Slices are honoured by seeking: a slice that starts in a later continuation block of a wide
//! partition jumps there via the block index, and rows outside the slices are skipped without
//! being decoded. Reversed reads walk the partition's blocks backwards.

use super::{BlockMeta, Entry, Header, SsTable, decode_row, read_entry};
use crate::clip::{Clipper, Item, Slices};
use crate::codec::BytesReader;
use crate::compare::{ClusteringComparator, Pos, compare_pos};
use crate::model::*;
use crate::tombstones::{Marker, dir};
use anyhow::{Result, bail};
use bytes::Bytes;
use std::cmp::Ordering;
use std::collections::VecDeque;
use std::ops::Range;
use std::sync::Arc;

/// Blocks a cursor needs before it can continue.
#[derive(Clone, Debug)]
pub struct BlockNeed {
    pub sst: Arc<SsTable>,
    pub blocks: Range<usize>,
}

pub enum Step<T> {
    Ready(T),
    Pending(BlockNeed),
}

/// The start or end of a key range scan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KeyBound {
    Unbounded,
    /// Before or after every key of a token.
    Token { token: i64, after: bool },
    Key { key: PartitionKey, inclusive: bool },
}

impl KeyBound {
    /// `key` is after this lower bound.
    pub fn admits_from(&self, key: &PartitionKey) -> bool {
        match self {
            KeyBound::Unbounded => true,
            KeyBound::Token { token, after } => if *after { key.token > *token } else { key.token >= *token },
            KeyBound::Key { key: k, inclusive } => if *inclusive { key >= k } else { key > k },
        }
    }

    /// `key` is before this upper bound.
    pub fn admits_to(&self, key: &PartitionKey) -> bool {
        match self {
            KeyBound::Unbounded => true,
            KeyBound::Token { token, after } => if *after { key.token <= *token } else { key.token < *token },
            KeyBound::Key { key: k, inclusive } => if *inclusive { key <= k } else { key < k },
        }
    }

    /// The smallest key this lower bound admits, as a search key for the block index.
    pub(crate) fn search_key(&self) -> Option<PartitionKey> {
        match self {
            KeyBound::Unbounded => None,
            KeyBound::Token { token, after: false } => Some(PartitionKey { token: *token, key: Bytes::new() }),
            KeyBound::Token { token, after: true } => Some(PartitionKey { token: token.saturating_add(1), key: Bytes::new() }),
            KeyBound::Key { key, .. } => Some(key.clone()),
        }
    }
}

enum Mode {
    Point(PartitionKey),
    Scan { start: KeyBound, end: KeyBound },
}

enum State {
    /// Looking for the next partition header.
    Headers,
    /// Inside a partition.
    Items { clip: Box<Clipper>, header: usize, last: usize, reversed: Option<Box<Reverse>> },
    /// The rest of the current partition is to be skipped.
    Skip,
    Done,
}

/// Reverse iteration over one partition: decoded entries of the current block, last first.
struct Reverse {
    block: usize,
    entries: Vec<RevEntry>,
    started: bool,
}

enum RevEntry {
    Row(Clustering, Range<usize>),
    Marker(Marker),
}

pub struct SstCursor {
    sst: Arc<SsTable>,
    cmp: Arc<dyn ClusteringComparator>,
    mode: Mode,
    slices: Slices,
    /// Only the partition header and static row (the clustering filter misses this SSTable).
    header_only: bool,
    readahead: usize,
    block: usize,
    data: Option<Bytes>,
    pos: usize,
    ahead: VecDeque<(usize, Bytes)>,
    state: State,
    started: bool,
    current_key: Option<PartitionKey>,
    /// A partition left after reading it backwards: `(header block, last block)`.
    rewind: Option<(usize, usize)>,
    /// Partition header offsets of the current block.
    offsets: super::HeaderOffsets,
    /// A point read hasn't binary-searched its header block yet.
    point_seek: bool,
    /// Skipping from the start of the header block: pass the partition's own header first.
    find_own_header: bool,
}

impl SstCursor {
    /// Read the partition `key` (the caller checked [`super::Index::may_contain`]).
    pub fn point(sst: Arc<SsTable>, cmp: Arc<dyn ClusteringComparator>, key: PartitionKey, slices: Slices) -> Self {
        Self::new(sst, cmp, Mode::Point(key), slices, 2)
    }

    /// Read every partition in `[start, end]` (forward only).
    pub fn scan(sst: Arc<SsTable>, cmp: Arc<dyn ClusteringComparator>, start: KeyBound, end: KeyBound, slices: Slices, readahead: usize) -> Self {
        Self::new(sst, cmp, Mode::Scan { start, end }, slices, readahead.max(1))
    }

    fn new(sst: Arc<SsTable>, cmp: Arc<dyn ClusteringComparator>, mode: Mode, slices: Slices, readahead: usize) -> Self {
        let point_seek = matches!(mode, Mode::Point(_));
        Self {
            sst,
            cmp,
            mode,
            slices,
            header_only: false,
            readahead,
            block: 0,
            data: None,
            pos: 0,
            ahead: VecDeque::new(),
            state: State::Headers,
            started: false,
            current_key: None,
            rewind: None,
            offsets: Default::default(),
            point_seek,
            find_own_header: false,
        }
    }

    pub fn header_only(mut self) -> Self {
        self.header_only = true;
        self
    }

    pub fn sstable(&self) -> &Arc<SsTable> {
        &self.sst
    }

    /// Hand over blocks fetched for a [`BlockNeed`].
    pub fn supply(&mut self, first: usize, blocks: Vec<Bytes>) {
        for (i, b) in blocks.into_iter().enumerate() {
            let n = first + i;
            if !self.ahead.iter().any(|(k, _)| *k == n) {
                self.ahead.push_back((n, b));
            }
        }
    }

    /// A point read found no partition: its bloom filter answered a false positive.
    fn missed(&self) {
        self.sst.bloom_false_positives.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    fn blocks(&self) -> &[BlockMeta] {
        &self.sst.index.blocks
    }

    /// Make block `i` current (at offset 0), or say what to fetch.
    fn load(&mut self, i: usize, upto: usize, backwards: bool) -> Result<Option<BlockNeed>> {
        if self.data.is_some() && self.block == i {
            return Ok(None);
        }
        if let Some(pos) = self.ahead.iter().position(|(k, _)| *k == i) {
            let (_, b) = self.ahead.remove(pos).expect("found");
            self.ahead.retain(|(k, _)| if backwards { *k < i } else { *k > i });
            let (entries, offsets) = super::split_block(&b).map_err(|e| anyhow::anyhow!("{} block {i}: {e}", self.sst.data_key()))?;
            self.block = i;
            self.data = Some(entries);
            self.offsets = offsets;
            self.pos = 0;
            return Ok(None);
        }
        let n = self.blocks().len();
        let range = if backwards {
            let lo = (i + 1).saturating_sub(self.readahead).max(upto);
            lo..i + 1
        } else {
            i..(i + self.readahead).min(upto + 1).min(n).max(i + 1)
        };
        Ok(Some(BlockNeed { sst: self.sst.clone(), blocks: range }))
    }

    /// The next entry, moving to the following block at the end of the current one.
    fn next_entry(&mut self, upto: usize) -> Result<Step<Option<Entry>>> {
        loop {
            if let Some(data) = &self.data {
                if self.pos < data.len() {
                    let mut r = BytesReader::at(data, self.pos)?;
                    let e = read_entry(&mut r)?;
                    self.pos = r.r.pos();
                    return Ok(Step::Ready(e));
                }
                let next = self.block + 1;
                if next >= self.blocks().len() {
                    return Ok(Step::Ready(None));
                }
                self.data = None;
                self.block = next;
            }
            let b = self.block;
            if let Some(need) = self.load(b, upto, false)? {
                return Ok(Step::Pending(need));
            }
        }
    }

    fn last_block(&self) -> usize {
        self.blocks().len().saturating_sub(1)
    }

    /// The next partition's header, or `None` at the end.
    pub fn next_header(&mut self) -> Result<Step<Option<Header>>> {
        loop {
            match &self.state {
                State::Done => return Ok(Step::Ready(None)),
                State::Items { .. } => {
                    self.skip_partition();
                    continue;
                }
                State::Skip => {
                    // A point read's partition starts in its header block: if the partition
                    // before it runs past that block, it isn't here.
                    if let (Mode::Point(_), Some(k)) = (&self.mode, &self.current_key)
                        && self.sst.index.last_block_of(k, self.block) > self.block
                    {
                        self.state = State::Done;
                        continue;
                    }
                    if let Some(need) = self.skip_rest()? {
                        return Ok(Step::Pending(need));
                    }
                    self.state = State::Headers;
                }
                State::Headers => {}
            }
            if !self.started {
                self.started = true;
                if self.blocks().is_empty() {
                    self.state = State::Done;
                    continue;
                }
                let first = match &self.mode {
                    Mode::Point(key) => match self.sst.index.header_block(key) {
                        Some(b) => b,
                        None => {
                            self.missed();
                            self.state = State::Done;
                            continue;
                        }
                    },
                    Mode::Scan { start, .. } => start.search_key().map_or(0, |k| self.sst.index.scan_block(&k)),
                };
                self.block = first;
                self.data = None;
            }
            if self.point_seek {
                let b = self.block;
                if let Some(need) = self.load(b, b, false)? {
                    return Ok(Step::Pending(need));
                }
                self.point_seek = false;
                let Mode::Point(key) = &self.mode else { unreachable!() };
                let data = self.data.clone().expect("loaded");
                // Binary search the block's partition headers for the key.
                let (mut lo, mut hi) = (0usize, self.offsets.len());
                while lo < hi {
                    let mid = (lo + hi) / 2;
                    if super::header_key_cmp(&data, self.offsets.get(mid), key)? == Ordering::Less {
                        lo = mid + 1;
                    } else {
                        hi = mid;
                    }
                }
                let found = (lo < self.offsets.len()).then(|| self.offsets.get(lo));
                match found {
                    Some(o) if super::header_key_cmp(&data, o, key)? == Ordering::Equal => self.pos = o as usize,
                    _ => {
                        self.missed();
                        self.state = State::Done;
                        continue;
                    }
                }
            }
            let upto = match &self.mode {
                Mode::Point(_) => {
                    if self.data.as_ref().is_some_and(|d| self.pos >= d.len()) {
                        self.state = State::Done;
                        continue;
                    }
                    self.block
                }
                Mode::Scan { .. } => self.last_block(),
            };
            let e = match self.next_entry(upto)? {
                Step::Pending(n) => return Ok(Step::Pending(n)),
                Step::Ready(e) => e,
            };
            let Some(e) = e else {
                self.state = State::Done;
                continue;
            };
            let Entry::Partition(h) = e else { continue };
            let header_block = self.block;
            let skip = match &self.mode {
                Mode::Point(key) => {
                    if h.key > *key {
                        self.missed();
                        self.state = State::Done;
                        continue;
                    }
                    h.key < *key
                }
                Mode::Scan { start, end } => {
                    if !end.admits_to(&h.key) {
                        self.state = State::Done;
                        continue;
                    }
                    !start.admits_from(&h.key)
                }
            };
            self.current_key = Some(h.key.clone());
            if skip {
                self.state = State::Skip;
                continue;
            }
            if self.header_only {
                self.state = match self.mode {
                    Mode::Point(_) => State::Done,
                    Mode::Scan { .. } => State::Skip,
                };
            } else {
                let last = self.sst.index.last_block_of(&h.key, header_block);
                let reversed = self.slices.reversed.then(|| Box::new(Reverse { block: usize::MAX, entries: Vec::new(), started: false }));
                self.state = State::Items { clip: Box::new(Clipper::new(self.slices.clone())), header: header_block, last, reversed };
            }
            return Ok(Step::Ready(Some(h)));
        }
    }

    /// Skip (forward) to just after the current partition's end entry.
    fn skip_rest(&mut self) -> Result<Option<BlockNeed>> {
        let Some(key) = self.current_key.clone() else { return Ok(None) };
        if let Some((header, last)) = self.rewind.take() {
            // Read backwards: continue from the start of the partition's last block.
            self.block = last;
            self.data = None;
            self.find_own_header = header == last;
        } else {
            let last = self.sst.index.last_block_of(&key, self.block);
            if last > self.block {
                self.block = last;
                self.data = None;
            }
        }
        loop {
            let upto = self.last_block();
            if self.find_own_header {
                match self.next_entry(upto)? {
                    Step::Pending(n) => return Ok(Some(n)),
                    Step::Ready(Some(Entry::Partition(h))) => self.find_own_header = h.key != key,
                    Step::Ready(None) => {
                        self.find_own_header = false;
                        self.current_key = None;
                        return Ok(None);
                    }
                    Step::Ready(Some(_)) => {}
                }
                continue;
            }
            match self.next_entry(upto)? {
                Step::Pending(n) => return Ok(Some(n)),
                Step::Ready(None) | Step::Ready(Some(Entry::End)) => {
                    self.current_key = None;
                    return Ok(None);
                }
                Step::Ready(Some(Entry::Partition(_))) => bail!("corrupt sstable {}: partition without an end", self.sst.key),
                Step::Ready(Some(_)) => {}
            }
        }
    }

    /// Stop reading the current partition (limits reached); the next `next_header` skips it.
    pub fn skip_partition(&mut self) {
        if let State::Items { header, last, reversed, .. } = &self.state {
            if reversed.is_some() {
                self.rewind = Some((*header, *last));
            }
            self.state = match self.mode {
                Mode::Point(_) => State::Done,
                Mode::Scan { .. } => State::Skip,
            };
        }
    }

    /// The current partition's next item, or `None` at its end.
    pub fn next_item(&mut self) -> Result<Step<Option<Item>>> {
        let cmp = self.cmp.clone();
        loop {
            let State::Items { clip, header, last, reversed } = &mut self.state else { return Ok(Step::Ready(None)) };
            if let Some(i) = clip.pop() {
                return Ok(Step::Ready(Some(i)));
            }
            if clip.done() {
                self.skip_partition();
                return Ok(Step::Ready(None));
            }
            let (header, last) = (*header, *last);
            if reversed.is_some() {
                match self.reverse_step(&*cmp, header, last)? {
                    Some(need) => return Ok(Step::Pending(need)),
                    None => continue,
                }
            }
            // Seek forward to the block holding the next slice.
            if let Some(target) = clip.seek_target()
                && last > self.block
            {
                let tb = self.sst.index.seek_block(&*cmp, header, last, target);
                if tb > self.block {
                    let open = self.blocks()[tb].cont.as_ref().and_then(|c| c.open);
                    let State::Items { clip, .. } = &mut self.state else { unreachable!() };
                    clip.set_open(open);
                    self.block = tb;
                    self.data = None;
                }
            }
            let e = match self.next_entry(last)? {
                Step::Pending(n) => return Ok(Step::Pending(n)),
                Step::Ready(e) => e,
            };
            let block = self.data.clone();
            let State::Items { clip, .. } = &mut self.state else { unreachable!() };
            match e {
                None | Some(Entry::End) => {
                    clip.finish();
                    if clip.has_output() {
                        let out = clip.pop();
                        self.after_end();
                        return Ok(Step::Ready(out));
                    }
                    self.after_end();
                    return Ok(Step::Ready(None));
                }
                Some(Entry::Partition(_)) => bail!("corrupt sstable {}: partition without an end", self.sst.key),
                Some(Entry::Marker(m)) => clip.push(&*cmp, Item::Marker(m)),
                Some(Entry::Row(c, range)) => {
                    if skippable(&*cmp, clip, &c) {
                        continue;
                    }
                    let row = decode_row(block.as_ref().expect("current block"), range)?;
                    clip.push(&*cmp, Item::Row(row));
                }
            }
        }
    }

    /// The partition's end entry was consumed.
    fn after_end(&mut self) {
        self.current_key = None;
        self.state = match self.mode {
            Mode::Point(_) => State::Done,
            Mode::Scan { .. } => State::Headers,
        };
    }

    /// Feed the clipper the next reversed entry; `Some` when a block must be fetched first.
    fn reverse_step(&mut self, cmp: &dyn ClusteringComparator, header: usize, last: usize) -> Result<Option<BlockNeed>> {
        let blocks_open = |s: &Self, k: usize| -> Option<DeletionTime> {
            // The deletion open at the forward end of block k.
            if k < last { s.blocks()[k + 1].cont.as_ref().and_then(|c| c.open) } else { None }
        };
        let State::Items { clip, reversed: Some(rev), .. } = &mut self.state else { return Ok(None) };
        if let Some(e) = rev.entries.pop() {
            match e {
                RevEntry::Marker(m) => clip.push(cmp, Item::Marker(m.reversed())),
                RevEntry::Row(c, range) => {
                    if !skippable(cmp, clip, &c) {
                        let row = decode_row(self.data.as_ref().expect("current block"), range)?;
                        clip.push(cmp, Item::Row(row));
                    }
                }
            }
            return Ok(None);
        }
        // Next block (backwards).
        let next = if !rev.started {
            let target = clip.seek_target().cloned().unwrap_or(Bound::TOP);
            self.sst.index.seek_block(cmp, header, last, &target)
        } else if rev.block > header {
            let mut k = rev.block - 1;
            if let Some(target) = clip.seek_target() {
                let tb = self.sst.index.seek_block(cmp, header, last, target);
                if tb < k {
                    k = tb;
                    rev.started = false;
                }
            }
            k
        } else {
            clip.finish();
            return Ok(None);
        };
        if let Some(need) = self.load(next, header, true)? {
            return Ok(Some(need));
        }
        let open = blocks_open(self, next);
        let data = self.data.clone().expect("loaded");
        let State::Items { clip, reversed: Some(rev), .. } = &mut self.state else { unreachable!() };
        if !rev.started {
            clip.set_open(open);
        }
        rev.started = true;
        rev.block = next;
        rev.entries.clear();
        let mut r = BytesReader::new(&data);
        // The header block starts the partition after its header entry; others start with it.
        let mut inside = next != header;
        while let Some(e) = read_entry(&mut r)? {
            match e {
                Entry::Partition(h) => {
                    if inside {
                        break;
                    }
                    inside = Some(&h.key) == self.current_key.as_ref();
                }
                Entry::End => {
                    if inside {
                        break;
                    }
                }
                Entry::Row(c, range) if inside => rev.entries.push(RevEntry::Row(c, range)),
                Entry::Marker(m) if inside => rev.entries.push(RevEntry::Marker(m)),
                _ => {}
            }
        }
        Ok(None)
    }
}

/// Whether a row can be skipped without decoding: before the slice being looked for.
fn skippable(cmp: &dyn ClusteringComparator, clip: &Clipper, c: &Clustering) -> bool {
    match clip.seek_target() {
        Some(target) => dir(compare_pos(cmp, Pos::Row(c), Pos::Bound(target)), clip.slices.reversed) == Ordering::Less,
        None => false,
    }
}
