//! The memory model of the module's document lookup, a `boost::unordered_flat_map` keyed by
//! document id: 15-slot groups with an overflow byte, power-of-two group counts, quadratic
//! probing from the mixed hash, growth at the maximum load (lowered by erases from overflowed
//! groups) and the allocator's byte counts.

use std::collections::HashMap;

const N: usize = 15;
/// `sizeof(std::pair<const t_docId, std::variant<point, polygon>>)`.
const VALUE: usize = 80;
const GROUP: usize = 16;

/// `mulx_mix` of the identity hash.
fn hash(id: u64) -> u64 {
    let r = (id as u128) * (0x9E37_79B9_7F4A_7C15u128);
    (r as u64) ^ ((r >> 64) as u64)
}

/// The reduced hash stored in a slot: the low byte, 0 and 1 (empty and sentinel) remapped.
fn reduced(h: u64) -> u8 {
    match h as u8 {
        0 => 8,
        1 => 9,
        b => b,
    }
}

#[derive(Default)]
pub struct FlatMap {
    /// log2 of the group count; 0 before the first allocation.
    bits: u32,
    slots: Vec<[u8; N]>,
    overflow: Vec<u8>,
    size: usize,
    ml: usize,
    at: HashMap<u64, (usize, usize)>,
}

/// The bytes the allocator hands out for `groups` groups (elements, then groups, aligned to
/// the element size).
fn buffer_bytes(groups: usize) -> usize {
    let raw = VALUE * (groups * N - 1) + GROUP * (groups + 1) - 1;
    raw.div_ceil(VALUE) * VALUE
}

impl FlatMap {
    fn capacity(&self) -> usize {
        if self.bits == 0 { 0 } else { (1usize << self.bits) * N - 1 }
    }

    fn initial_ml(&self) -> usize {
        let c = self.capacity();
        if c < 2 * N { c } else { (0.875f32 * c as f32) as usize }
    }

    fn place(&mut self, id: u64) {
        let h = hash(id);
        let groups = 1usize << self.bits;
        let mask = groups - 1;
        let mut pos = (h >> (64 - self.bits)) as usize;
        let mut step = 0;
        loop {
            // The sentinel occupies the last group's last slot.
            let avail = (0..N).find(|&s| self.slots[pos][s] == 0 && !(pos == mask && s == N - 1));
            if let Some(s) = avail {
                self.slots[pos][s] = reduced(h);
                self.at.insert(id, (pos, s));
                return;
            }
            self.overflow[pos] |= 1 << (h % 8);
            step += 1;
            pos = (pos + step) & mask;
        }
    }

    /// Inserts `id` (absent); returns the change in allocated bytes.
    pub fn insert(&mut self, id: u64) -> i64 {
        if self.at.contains_key(&id) {
            return 0;
        }
        let mut delta = 0i64;
        if self.size < self.ml {
            self.place(id);
        } else {
            // `new_arrays_for_growth`, the new element placed first, then the old ones in
            // group and slot order.
            let n = ((self.size + self.size / 61 + 1) as f32 / 0.875f32).ceil() as usize;
            let want = n / N + 1;
            let groups = want.next_power_of_two().max(2);
            let old_groups = if self.bits == 0 { 0 } else { 1usize << self.bits };
            let mut order: Vec<(usize, usize, u64)> = self.at.iter().map(|(&k, &(g, s))| (g, s, k)).collect();
            order.sort_unstable();
            self.bits = groups.trailing_zeros();
            self.slots = vec![[0u8; N]; groups];
            self.overflow = vec![0u8; groups];
            self.at.clear();
            self.place(id);
            for (_, _, k) in order {
                self.place(k);
            }
            self.ml = self.initial_ml();
            delta += buffer_bytes(groups) as i64;
            if old_groups > 0 {
                delta -= buffer_bytes(old_groups) as i64;
            }
        }
        self.size += 1;
        delta
    }

    /// Erases `id`: the slot freed, the maximum load lowered when its group overflowed.
    pub fn erase(&mut self, id: u64) {
        if let Some((g, s)) = self.at.remove(&id) {
            let b = self.slots[g][s];
            if self.overflow[g] & (1 << (b % 8)) != 0 {
                self.ml -= 1;
            }
            self.slots[g][s] = 0;
            self.size -= 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn growth() {
        let mut m = FlatMap::default();
        let mut total = 0;
        let mut steps = Vec::new();
        for id in 1..=60u64 {
            let d = m.insert(id);
            total += d;
            if d != 0 {
                steps.push((id, d));
            }
        }
        assert_eq!(steps, vec![(1, 2400), (30, 2400), (52, 4880)]);
        assert_eq!(total, 9680);
    }
}
