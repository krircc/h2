use super::Header;

use fnv::FnvHasher;
use http::method;
use http::header::{self, HeaderName, HeaderValue};

use std::{cmp, mem, usize};
use std::collections::VecDeque;
use std::hash::{Hash, Hasher};

pub struct Table {
    mask: usize,
    indices: Vec<Option<Pos>>,
    slots: VecDeque<Slot>,
    // This tracks the number of evicted elements. It is expected to wrap. This
    // value is used to map `Pos::index` to the actual index in the VecDeque.
    evicted: usize,
    // Size is in bytes
    size: usize,
    max_size: usize,
}

#[derive(Debug)]
pub enum Index<'a> {
    // The header is already fully indexed
    Indexed(usize, Header),

    // The name is indexed, but not the value
    Name(usize, Header),

    // The full header has been inserted into the table.
    Inserted(&'a Header),

    // Only the value has been inserted
    InsertedValue(usize, &'a Header),

    // The header is not indexed by this table
    NotIndexed(Header),
}

struct Slot {
    hash: HashValue,
    header: Header,
    next: Option<usize>,
}

#[derive(Debug, Clone, Copy)]
struct Pos {
    index: usize,
    hash: HashValue,
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
struct HashValue(usize);

const MAX_SIZE: usize = (1 << 16);
const DYN_OFFSET: usize = 62;

macro_rules! probe_loop {
    ($probe_var: ident < $len: expr, $body: expr) => {
        debug_assert!($len > 0);
        loop {
            if $probe_var < $len {
                $body
                $probe_var += 1;
            } else {
                $probe_var = 0;
            }
        }
    };
}

impl Table {
    pub fn new(max_size: usize, capacity: usize) -> Table {
        if capacity == 0 {
            Table {
                mask: 0,
                indices: vec![],
                slots: VecDeque::new(),
                evicted: 0,
                size: 0,
                max_size: max_size,
            }
        } else {
            let capacity = cmp::max(
                to_raw_capacity(capacity).next_power_of_two(),
                8);

            Table {
                mask: capacity.wrapping_sub(1),
                indices: vec![None; capacity],
                slots: VecDeque::with_capacity(usable_capacity(capacity)),
                evicted: 0,
                size: 0,
                max_size: max_size,
            }
        }
    }

    #[inline]
    pub fn capacity(&self) -> usize {
        usable_capacity(self.indices.len())
    }

    pub fn max_size(&self) -> usize {
        self.max_size
    }

    /// Index the header in the HPACK table.
    pub fn index(&mut self, header: Header) -> Index {
        // Check the static table
        let statik = index_static(&header);

        // Don't index certain headers. This logic is borrowed from nghttp2.
        if header.skip_value_index() {
            return Index::new(statik, header);
        }

        // If the header is already indexed by the static table, return that
        if let Some((n, true)) = statik {
            return Index::Indexed(n, header);
        }

        // Don't index large headers
        if header.len() * 4 > self.max_size * 3 {
            return Index::new(statik, header);
        }

        self.index_dynamic(header, statik)
    }

    fn index_dynamic(&mut self, header: Header, statik: Option<(usize, bool)>) -> Index {
        if header.len() + self.size < self.max_size || !header.is_sensitive() {
            // Only grow internal storage if needed
            self.reserve_one();
        }

        if self.indices.is_empty() {
            // If `indices` is not empty, then it is impossible for all
            // `indices` entries to be `Some`. So, we only need to check for the
            // empty case.
            return Index::new(statik, header);
        }

        let hash = hash_header(&header);

        let desired_pos = desired_pos(self.mask, hash);
        let mut probe = desired_pos;
        let mut dist = 0;

        // Start at the ideal position, checking all slots
        probe_loop!(probe < self.indices.len(), {
            if let Some(pos) = self.indices[probe] {
                // The slot is already occupied, but check if it has a lower
                // displacement.
                let their_dist = probe_distance(self.mask, pos.hash, probe);

                let slot_idx = pos.index.wrapping_sub(self.evicted);

                if their_dist < dist {
                    // Index robinhood
                    return self.index_vacant(header, hash, dist, probe, statik);
                } else if pos.hash == hash && self.slots[slot_idx].header.name() == header.name() {
                    // Matching name, check values
                    return self.index_occupied(header, hash, pos.index, statik);
                }
            } else {
                return self.index_vacant(header, hash, dist, probe, statik);
            }

            dist += 1;
        });
    }

    fn index_occupied(&mut self,
                      header: Header,
                      hash: HashValue,
                      mut index: usize,
                      statik: Option<(usize, bool)>)
        -> Index
    {
        // There already is a match for the given header name. Check if a value
        // matches. The header will also only be inserted if the table is not at
        // capacity.
        loop {
            // Compute the real index into the VecDeque
            let real_idx = index.wrapping_sub(self.evicted);

            if self.slots[real_idx].header.value_eq(&header) {
                // We have a full match!
                return Index::Indexed(real_idx + DYN_OFFSET, header);
            }

            if let Some(next) = self.slots[real_idx].next {
                index = next;
                continue;
            }

            if header.is_sensitive() {
                return Index::Name(real_idx + DYN_OFFSET, header);
            }

            self.update_size(header.len(), index);

            let new_idx = self.slots.len();

            // If `evicted` is greater than `index`, then the previous node in
            // the linked list got evicted. The header we are about to insert is
            // the new "head" of the list and `indices` has already been
            // updated. So, all that is left to do is insert the header in the
            // VecDeque.
            //
            // TODO: This logic isn't correct in the face of wrapping
            if self.evicted <= index {
                // Recompute `real_idx` since this could have been modified by
                // entries being evicted
                let real_idx = index.wrapping_sub(self.evicted);

                self.slots[real_idx].next = Some(new_idx.wrapping_add(self.evicted));
            }

            self.slots.push_back(Slot {
                hash: hash,
                header: header,
                next: None,
            });

            // Even if the previous header was evicted, we can still reference
            // it when inserting the new one...
            return Index::InsertedValue(real_idx + DYN_OFFSET, &self.slots[new_idx].header);
        }

        Index::NotIndexed(header)
    }

    fn index_vacant(&mut self,
                    header: Header,
                    hash: HashValue,
                    dist: usize,
                    mut probe: usize,
                    statik: Option<(usize, bool)>)
        -> Index
    {
        if header.is_sensitive() {
            return Index::new(statik, header);
        }

        // Passing in `usize::MAX` for prev_idx since there is no previous
        // header in this case.
        if self.update_size(header.len(), usize::MAX) {
            if dist != 0 {
                let back = probe.wrapping_sub(1) & self.mask;

                if let Some(pos) = self.indices[probe] {
                    let their_dist = probe_distance(self.mask, pos.hash, probe);

                    if their_dist < dist {
                        probe = back;
                    }
                } else {
                    probe = back;
                }
            }
        }

        // The index is offset by the current # of evicted elements
        let slot_idx = self.slots.len();
        let pos_idx = slot_idx.wrapping_add(self.evicted);

        self.slots.push_back(Slot {
            hash: hash,
            header: header,
            next: None,
        });

        let mut prev = mem::replace(&mut self.indices[probe], Some(Pos {
            index: pos_idx,
            hash: hash,
        }));

        if let Some(mut prev) = prev {
            // Shift forward
            let mut probe = probe + 1;

            probe_loop!(probe < self.indices.len(), {
                let pos = &mut self.indices[probe as usize];

                prev = match mem::replace(pos, Some(prev)) {
                    Some(p) => p,
                    None => break,
                };
            });
        }

        if let Some((n, _)) = statik {
            Index::InsertedValue(n, &self.slots[slot_idx].header)
        } else {
            Index::Inserted(&self.slots[slot_idx].header)
        }
    }

    pub fn resize(&mut self, size: usize) {
        self.max_size = size;

        if size == 0 {
            self.size = 0;

            for i in &mut self.indices {
                *i = None;
            }

            self.slots.clear();
            self.evicted = 0;
        } else {
            self.converge(usize::MAX);
        }
    }

    fn update_size(&mut self, len: usize, prev_idx: usize) -> bool {
        self.size += len;
        self.converge(prev_idx)
    }

    fn converge(&mut self, prev_idx: usize) -> bool {
        let mut ret = false;

        while self.size > self.max_size {
            ret = true;
            self.evict(prev_idx);
        }

        ret
    }

    fn evict(&mut self, prev_idx: usize) {
        debug_assert!(!self.slots.is_empty());

        // Remove the header
        let slot = self.slots.pop_front().unwrap();
        let mut probe = desired_pos(self.mask, slot.hash);

        // Update the size
        self.size -= slot.header.len();

        // Equivalent to 0.wrapping_add(self.evicted);
        let pos_idx = self.evicted;

        // Find the associated position
        probe_loop!(probe < self.indices.len(), {
            let mut pos = self.indices[probe].unwrap();

            if pos.index == pos_idx {
                if let Some(idx) = slot.next {
                    pos.index = idx;
                    self.indices[probe] = Some(pos);
                } else if pos.index == prev_idx {
                    pos.index = (self.slots.len() + 1).wrapping_add(self.evicted);
                    self.indices[probe] = Some(pos);
                } else {
                    self.indices[probe] = None;
                    self.remove_phase_two(probe);
                }

                break;
            }
        });

        self.evicted = self.evicted.wrapping_add(1);
    }

    // Shifts all indices that were displaced by the header that has just been
    // removed.
    fn remove_phase_two(&mut self, probe: usize) {
        let mut last_probe = probe;
        let mut probe = probe + 1;

        probe_loop!(probe < self.indices.len(), {
            if let Some(pos) = self.indices[probe] {
                if probe_distance(self.mask, pos.hash, probe) > 0 {
                    self.indices[last_probe] = self.indices[probe].take();
                } else {
                    break;
                }
            } else {
                break;
            }

            last_probe = probe;
        });
    }

    fn reserve_one(&mut self) {
        let len = self.slots.len();

        if len == self.capacity() {
            if len == 0 {
                let new_raw_cap = 8;
                self.mask = 8 - 1;
                self.indices = vec![None; new_raw_cap];
            } else {
                let raw_cap = self.indices.len();
                self.grow(raw_cap << 1);
            }
        }
    }

    #[inline]
    fn grow(&mut self, new_raw_cap: usize) {
        // This path can never be reached when handling the first allocation in
        // the map.

        // find first ideally placed element -- start of cluster
        let mut first_ideal = 0;

        for (i, pos) in self.indices.iter().enumerate() {
            if let Some(pos) = *pos {
                if 0 == probe_distance(self.mask, pos.hash, pos.index) {
                    first_ideal = i;
                    break;
                }
            }
        }

        // visit the entries in an order where we can simply reinsert them
        // into self.indices without any bucket stealing.
        let old_indices = mem::replace(&mut self.indices, vec![None; new_raw_cap]);
        self.mask = new_raw_cap.wrapping_sub(1);

        for &pos in &old_indices[first_ideal..] {
            self.reinsert_entry_in_order(pos);
        }

        for &pos in &old_indices[..first_ideal] {
            self.reinsert_entry_in_order(pos);
        }
    }

    fn reinsert_entry_in_order(&mut self, pos: Option<Pos>) {
        if let Some(pos) = pos {
            // Find first empty bucket and insert there
            let mut probe = desired_pos(self.mask, pos.hash);

            probe_loop!(probe < self.indices.len(), {
                if self.indices[probe as usize].is_none() {
                    // empty bucket, insert here
                    self.indices[probe as usize] = Some(pos);
                    return;
                }
            });
        }
    }
}

#[cfg(test)]
impl Table {
    /// Returns the number of headers in the table
    pub fn len(&self) -> usize {
        self.slots.len()
    }

    /// Returns the table size
    pub fn size(&self) -> usize {
        self.size
    }
}

impl<'a> Index<'a> {
    fn new(v: Option<(usize, bool)>, e: Header) -> Index<'a> {
        match v {
            None => Index::NotIndexed(e),
            Some((n, true)) => Index::Indexed(n, e),
            Some((n, false)) => Index::Name(n, e),
        }
    }
}

#[inline]
fn usable_capacity(cap: usize) -> usize {
    cap - cap / 4
}

#[inline]
fn to_raw_capacity(n: usize) -> usize {
    n + n / 3
}

#[inline]
fn desired_pos(mask: usize, hash: HashValue) -> usize {
    (hash.0 & mask) as usize
}

#[inline]
fn probe_distance(mask: usize, hash: HashValue, current: usize) -> usize {
    current.wrapping_sub(desired_pos(mask, hash)) & mask as usize
}

fn hash_header(header: &Header) -> HashValue {
    const MASK: u64 = (MAX_SIZE as u64) - 1;

    let mut h = FnvHasher::default();
    header.name().hash(&mut h);
    HashValue((h.finish() & MASK) as usize)
}

/// Checks the static table for the header. If found, returns the index and a
/// boolean representing if the value matched as well.
fn index_static(header: &Header) -> Option<(usize, bool)> {
    match *header {
        Header::Field { ref name, ref value } => {
            match *name {
                header::ACCEPT_CHARSET => Some((15, false)),
                header::ACCEPT_ENCODING => {
                    if value == "gzip, deflate" {
                        Some((16, true))
                    } else {
                        Some((16, false))
                    }
                }
                header::ACCEPT_LANGUAGE => Some((17, false)),
                header::ACCEPT_RANGES => Some((18, false)),
                header::ACCEPT => Some((19, false)),
                header::ACCESS_CONTROL_ALLOW_ORIGIN => Some((20, false)),
                header::AGE => Some((21, false)),
                header::ALLOW => Some((22, false)),
                header::AUTHORIZATION => Some((23, false)),
                header::CACHE_CONTROL => Some((24, false)),
                header::CONTENT_DISPOSITION => Some((25, false)),
                header::CONTENT_ENCODING => Some((26, false)),
                header::CONTENT_LANGUAGE => Some((27, false)),
                header::CONTENT_LENGTH => Some((28, false)),
                header::CONTENT_LOCATION => Some((29, false)),
                header::CONTENT_RANGE => Some((30, false)),
                header::CONTENT_TYPE => Some((31, false)),
                header::COOKIE => Some((32, false)),
                header::DATE => Some((33, false)),
                header::ETAG => Some((34, false)),
                header::EXPECT => Some((35, false)),
                header::EXPIRES => Some((36, false)),
                header::FROM => Some((37, false)),
                header::HOST => Some((38, false)),
                header::IF_MATCH => Some((39, false)),
                header::IF_MODIFIED_SINCE => Some((40, false)),
                header::IF_NONE_MATCH => Some((41, false)),
                header::IF_RANGE => Some((42, false)),
                header::IF_UNMODIFIED_SINCE => Some((43, false)),
                header::LAST_MODIFIED => Some((44, false)),
                header::LINK => Some((45, false)),
                header::LOCATION => Some((46, false)),
                header::MAX_FORWARDS => Some((47, false)),
                header::PROXY_AUTHENTICATE => Some((48, false)),
                header::PROXY_AUTHORIZATION => Some((49, false)),
                header::RANGE => Some((50, false)),
                header::REFERER => Some((51, false)),
                header::REFRESH => Some((52, false)),
                header::RETRY_AFTER => Some((53, false)),
                header::SERVER => Some((54, false)),
                header::SET_COOKIE => Some((55, false)),
                header::STRICT_TRANSPORT_SECURITY => Some((56, false)),
                header::TRANSFER_ENCODING => Some((57, false)),
                header::USER_AGENT => Some((58, false)),
                header::VARY => Some((59, false)),
                header::VIA => Some((60, false)),
                header::WWW_AUTHENTICATE => Some((61, false)),
                _ => None,
            }
        }
        Header::Authority(ref v) => Some((1, false)),
        Header::Method(ref v) => {
            match *v {
                method::GET => Some((2, true)),
                method::POST => Some((3, true)),
                _ => Some((2, false)),
            }
        }
        Header::Scheme(ref v) => {
            match &**v {
                "http" => Some((6, true)),
                "https" => Some((7, true)),
                _ => Some((6, false)),
            }
        }
        Header::Path(ref v) => {
            match &**v {
                "/" => Some((4, true)),
                "/index.html" => Some((5, true)),
                _ => Some((4, false)),
            }
        }
        Header::Status(ref v) => {
            match u16::from(*v) {
                200 => Some((8, true)),
                204 => Some((9, true)),
                206 => Some((10, true)),
                304 => Some((11, true)),
                400 => Some((12, true)),
                404 => Some((13, true)),
                500 => Some((14, true)),
                _ => Some((8, false)),
            }
        }
    }
}
