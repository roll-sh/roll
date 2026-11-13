//! Intersecting dense postings blocks.
//!
//! The codec stores a postings block whose 128 docs fall within 4096 doc ids as a bit set
//! (`PostingsCursor::dense_block`). Frequent terms are mostly such blocks, and a conjunction of
//! frequent terms (a phrase's approximation, `+the +and`) spends its time leapfrogging cursors
//! that agree on most docs. ANDing the blocks' words finds the next doc they share 64 doc ids
//! at a time; the cursors then only seek within their current blocks.

/// A dense block: `(start, words, last)`, bit `i` of `words` being doc `start + i`.
pub type DenseBlock<'a> = (u32, &'a [u64], u32);

/// The 64 bits of `block` for docs `x..x + 64` (bit `j` is doc `x + j`).
#[inline]
fn bits64(block: &DenseBlock<'_>, x: u32) -> u64 {
    let (start, words, _) = *block;
    if x < start {
        let sh = start - x;
        if sh >= 64 {
            return 0;
        }
        return words.first().map_or(0, |&w| w << sh);
    }
    let rel = (x - start) as usize;
    let (wi, o) = (rel >> 6, rel & 63);
    let lo = words.get(wi).map_or(0, |&w| w >> o);
    let hi = if o == 0 { 0 } else { words.get(wi + 1).map_or(0, |&w| w << (64 - o)) };
    lo | hi
}

/// The first doc `>= target` set in every block, looking no further than the smallest block
/// end: `Ok(doc)`, or `Err(end)` when they share no doc in `target..=end`.
pub fn first_common(blocks: &[DenseBlock<'_>], target: u32) -> Result<u32, u32> {
    let end = blocks.iter().map(|b| b.2).min().unwrap_or(0);
    let mut x = target;
    while x <= end {
        let mut m = u64::MAX;
        for b in blocks {
            m &= bits64(b, x);
            if m == 0 {
                break;
            }
        }
        let span = end - x;
        if span < 63 {
            m &= (1u64 << (span + 1)) - 1;
        }
        if m != 0 {
            return Ok(x + m.trailing_zeros());
        }
        match x.checked_add(64) {
            Some(n) => x = n,
            None => break,
        }
    }
    Err(end)
}

#[cfg(test)]
mod tests {
    use rand::rngs::StdRng;
    use rand::{Rng, SeedableRng};

    use super::*;

    fn block(docs: &[u32]) -> (u32, Vec<u64>, u32) {
        let start = docs[0];
        let last = *docs.last().unwrap();
        let mut words = vec![0u64; ((last - start) as usize >> 6) + 1];
        for &d in docs {
            let r = (d - start) as usize;
            words[r >> 6] |= 1 << (r & 63);
        }
        (start, words, last)
    }

    #[test]
    fn matches_a_brute_force_intersection() {
        let mut rng = StdRng::seed_from_u64(5);
        for _ in 0..2000 {
            let n = rng.gen_range(1..4);
            let sets: Vec<Vec<u32>> = (0..n)
                .map(|_| {
                    let start = rng.gen_range(0..300u32);
                    let p = rng.gen_range(0.05..1.0);
                    let mut v: Vec<u32> = (start..start + rng.gen_range(1..800)).filter(|_| rng.gen_bool(p)).collect();
                    if v.is_empty() {
                        v.push(start);
                    }
                    v
                })
                .collect();
            let blocks: Vec<(u32, Vec<u64>, u32)> = sets.iter().map(|s| block(s)).collect();
            let views: Vec<DenseBlock<'_>> = blocks.iter().map(|(s, w, l)| (*s, w.as_slice(), *l)).collect();
            let target = rng.gen_range(0..900u32);
            let end = blocks.iter().map(|b| b.2).min().unwrap();
            let want = (target..=end).find(|d| sets.iter().all(|s| s.binary_search(d).is_ok()));
            assert_eq!(first_common(&views, target), want.ok_or(end), "{sets:?} from {target}");
        }
    }

    #[test]
    fn edges() {
        let (s, w, l) = block(&[u32::MAX - 70, u32::MAX - 1]);
        assert_eq!(first_common(&[(s, &w, l)], u32::MAX - 69), Ok(u32::MAX - 1));
        assert_eq!(first_common(&[(s, &w, l)], u32::MAX), Err(u32::MAX - 1));
        let (s, w, l) = block(&[10, 200]);
        assert_eq!(first_common(&[(s, &w, l)], 0), Ok(10));
        assert_eq!(first_common(&[(s, &w, l)], 11), Ok(200));
    }
}
