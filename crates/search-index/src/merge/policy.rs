//! Merge selection: Lucene 10's `TieredMergePolicy` with Elasticsearch 9's defaults
//! (`index.merge.policy.*`), and `LogByteSizeMergePolicy` for time-based indices.
//!
//! Tiered: segments are sized by bytes discounted by their deletion ratio; the policy computes
//! how many segments the index "should" have (tiers of `segments_per_tier` segments, each tier
//! `merge_factor` times bigger, floored at `floor_segment`) and, while there are more (or too
//! many deletions), picks the cheapest-scoring run of size-sorted segments: low skew (similar
//! sizes), small result and many deletions reclaimed score best. Segments over half the max
//! merged size are left alone unless they carry too many deletions. Forced merges
//! (`_forcemerge`) and expunge-deletes follow `findForcedMerges` / `findForcedDeletesMerges`.

/// `index.merge.policy.*` settings.
#[derive(Clone, Debug, PartialEq)]
pub struct MergePolicyConfig {
    pub kind: PolicyKind,
    /// `floor_segment` (16 MB).
    pub floor_segment_bytes: u64,
    /// `max_merge_at_once` (16).
    pub max_merge_at_once: usize,
    /// `max_merged_segment` (5 GB; 100 GB for log byte size).
    pub max_merged_segment_bytes: u64,
    /// `segments_per_tier` (8).
    pub segments_per_tier: f64,
    /// `deletes_pct_allowed` (20).
    pub deletes_pct_allowed: f64,
    /// `expunge_deletes_allowed` (10).
    pub expunge_deletes_allowed: f64,
    /// `merge_factor` of log byte size (32).
    pub merge_factor: usize,
    /// Lucene's `targetSearchConcurrency` (1).
    pub target_search_concurrency: usize,
}

/// Which policy (`index.merge.policy.type`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PolicyKind {
    Tiered,
    LogByteSize,
}

impl Default for MergePolicyConfig {
    fn default() -> Self {
        MergePolicyConfig {
            kind: PolicyKind::Tiered,
            floor_segment_bytes: 16 << 20,
            max_merge_at_once: 16,
            max_merged_segment_bytes: 5 << 30,
            segments_per_tier: 8.0,
            deletes_pct_allowed: 20.0,
            expunge_deletes_allowed: 10.0,
            merge_factor: 32,
            target_search_concurrency: 1,
        }
    }
}

impl MergePolicyConfig {
    /// Elasticsearch's choice for time-based data (data streams): log byte size, 100 GB.
    pub fn time_based() -> Self {
        MergePolicyConfig { kind: PolicyKind::LogByteSize, max_merged_segment_bytes: 100 << 30, ..Default::default() }
    }
}

/// What the policy knows about a segment.
#[derive(Clone, Copy, Debug)]
pub struct SegInfo {
    /// Caller's handle (index into its list).
    pub id: usize,
    pub size_bytes: u64,
    pub max_doc: u32,
    pub del_count: u32,
    /// Already being merged.
    pub merging: bool,
    /// Tie-breaker standing in for Lucene's segment name order (older first).
    pub order: u64,
    /// For forced merges: present when the force merge started (Lucene's `isOriginal`), not
    /// produced by it. Deletions only force a merge of original segments; natural merges
    /// ignore this.
    pub original: bool,
}

impl SegInfo {
    /// Bytes discounted by the deletion ratio (`MergePolicy.size`).
    fn size(&self) -> u64 {
        if self.max_doc == 0 {
            return self.size_bytes;
        }
        let r = self.del_count as f64 / self.max_doc as f64;
        (self.size_bytes as f64 * (1.0 - r)) as u64
    }
}

#[derive(Clone, Copy)]
struct Sized {
    info: SegInfo,
    size: u64,
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum MergeType {
    Natural,
    ForceMergeDeletes,
}

fn sorted_by_size(infos: &[SegInfo]) -> Vec<Sized> {
    let mut v: Vec<Sized> = infos.iter().map(|i| Sized { info: *i, size: i.size() }).collect();
    v.sort_by(|a, b| b.size.cmp(&a.size).then(a.info.order.cmp(&b.info.order)));
    v
}

impl MergePolicyConfig {
    fn floor(&self, bytes: u64) -> u64 {
        bytes.max(self.floor_segment_bytes)
    }

    fn max_allowed_docs(&self, total_max_doc: i64, total_del: i64) -> i64 {
        let t = self.target_search_concurrency.max(1) as i64;
        (total_max_doc - total_del + t - 1) / t
    }

    /// Natural merges: lists of segment ids (callers' handles) to merge.
    pub fn find_merges(&self, infos: &[SegInfo]) -> Vec<Vec<usize>> {
        match self.kind {
            PolicyKind::Tiered => self.tiered_find_merges(infos),
            PolicyKind::LogByteSize => self.log_find_merges(infos),
        }
    }

    fn tiered_find_merges(&self, infos: &[SegInfo]) -> Vec<Vec<usize>> {
        let mut tot_bytes: i64 = 0;
        let mut min_seg = u64::MAX;
        let mut total_del: i64 = 0;
        let mut total_max_doc: i64 = 0;
        let mut merging_bytes: u64 = 0;
        let mut sorted = Vec::new();
        for s in sorted_by_size(infos) {
            if s.info.merging {
                merging_bytes += s.size;
                total_max_doc += (s.info.max_doc - s.info.del_count) as i64;
            } else {
                total_del += s.info.del_count as i64;
                total_max_doc += s.info.max_doc as i64;
                sorted.push(s);
            }
            min_seg = min_seg.min(s.size);
            tot_bytes += s.size as i64;
        }
        if total_max_doc == 0 {
            return Vec::new();
        }
        let total_del_pct = 100.0 * total_del as f64 / total_max_doc as f64;
        let mut allowed_del = (self.deletes_pct_allowed * total_max_doc as f64 / 100.0) as i64;
        let mut too_big = 0usize;
        let mut concurrency = 0usize;
        let mut allowed_seg: f64 = 0.0;
        let max_merged = self.max_merged_segment_bytes;
        let mut keep = Vec::with_capacity(sorted.len());
        for s in sorted {
            let seg_del_pct = 100.0 * s.info.del_count as f64 / s.info.max_doc.max(1) as f64;
            if s.size > max_merged / 2
                && (total_del_pct <= self.deletes_pct_allowed || seg_del_pct <= self.deletes_pct_allowed)
            {
                too_big += 1;
                tot_bytes -= s.size as i64;
                allowed_del -= s.info.del_count as i64;
                continue;
            } else if concurrency + too_big < self.target_search_concurrency.saturating_sub(1) {
                concurrency += 1;
                allowed_seg += 1.0;
                tot_bytes -= s.size as i64;
            }
            keep.push(s);
        }
        let allowed_del = allowed_del.max(0);
        let merge_factor = (self.max_merge_at_once as f64).min(self.segments_per_tier) as usize;
        let mut level = min_seg.max(self.floor_segment_bytes);
        let mut left = tot_bytes.max(0) as f64;
        loop {
            let count_level = left / level as f64;
            if count_level < self.segments_per_tier || level == max_merged {
                allowed_seg += count_level.ceil();
                break;
            }
            allowed_seg += self.segments_per_tier;
            left -= self.segments_per_tier * level as f64;
            level = max_merged.min(level.saturating_mul(merge_factor as u64));
        }
        allowed_seg = allowed_seg.max(self.segments_per_tier);
        allowed_seg = allowed_seg.max(self.target_search_concurrency as f64 - too_big as f64);
        let allowed_docs = self.max_allowed_docs(total_max_doc, total_del);
        self.do_find_merges(
            keep,
            merge_factor,
            allowed_seg as i64,
            allowed_del,
            allowed_docs,
            MergeType::Natural,
            merging_bytes >= max_merged,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn do_find_merges(
        &self,
        mut eligible: Vec<Sized>,
        merge_factor: usize,
        allowed_seg: i64,
        allowed_del: i64,
        allowed_docs: i64,
        mt: MergeType,
        max_merge_running: bool,
    ) -> Vec<Vec<usize>> {
        let mut spec = Vec::new();
        if eligible.is_empty() {
            return spec;
        }
        let max_merged = self.max_merged_segment_bytes;
        let mut have_large = false;
        let mut to_merge: Vec<usize> = Vec::new();
        loop {
            eligible.retain(|s| !to_merge.contains(&s.info.id));
            if eligible.is_empty() {
                return spec;
            }
            let remaining_del: i64 = eligible.iter().map(|s| s.info.del_count as i64).sum();
            if mt == MergeType::Natural && (eligible.len() as i64) <= allowed_seg && remaining_del <= allowed_del {
                return spec;
            }
            let mut best: Option<(f64, Vec<usize>, bool)> = None;
            for start in 0..eligible.len() {
                let mut cand: Vec<usize> = Vec::new();
                let mut too_large = false;
                let mut bytes = 0u64;
                let mut docs = 0i64;
                let mut idx = start;
                while idx < eligible.len()
                    && cand.len() < self.max_merge_at_once
                    && (cand.len() < merge_factor || bytes < self.floor_segment_bytes)
                    && bytes < max_merged
                    && (bytes < self.floor_segment_bytes || docs <= allowed_docs)
                {
                    let s = &eligible[idx];
                    let seg_docs = (s.info.max_doc - s.info.del_count) as i64;
                    if bytes + s.size > max_merged
                        || (bytes > self.floor_segment_bytes && docs + seg_docs > allowed_docs)
                    {
                        too_large |= bytes + s.size > max_merged;
                        if !cand.is_empty() {
                            idx += 1;
                            continue;
                        }
                    }
                    cand.push(idx);
                    bytes += s.size;
                    docs += seg_docs;
                    idx += 1;
                }
                let first = &eligible[cand[0]];
                if !too_large
                    && mt == MergeType::Natural
                    && (bytes as f64) < first.size as f64 * 1.5
                    && (first.info.del_count as f64) < first.info.max_doc as f64 * self.deletes_pct_allowed / 100.0
                {
                    continue;
                }
                if cand.len() == 1 && first.info.del_count == 0 {
                    continue;
                }
                if best.is_some() && !too_large && cand.len() < merge_factor {
                    break;
                }
                let score = self.score(&eligible, &cand, too_large);
                let better = best.as_ref().is_none_or(|b| score < b.0);
                if better && (!too_large || !max_merge_running) {
                    best = Some((score, cand.iter().map(|&i| eligible[i].info.id).collect(), too_large));
                }
            }
            let Some((_, ids, too_large)) = best else { return spec };
            if !have_large || !too_large || mt == MergeType::ForceMergeDeletes {
                have_large |= too_large;
                spec.push(ids.clone());
            }
            to_merge.extend(ids);
        }
    }

    fn score(&self, eligible: &[Sized], cand: &[usize], too_large: bool) -> f64 {
        let mut before = 0u64;
        let mut after = 0u64;
        let mut after_floored = 0u64;
        for &i in cand {
            after += eligible[i].size;
            after_floored += self.floor(eligible[i].size);
            before += eligible[i].info.size_bytes;
        }
        let skew = if too_large {
            let mf = (self.max_merge_at_once as f64).min(self.segments_per_tier) as usize;
            1.0 / mf as f64
        } else {
            self.floor(eligible[cand[0]].size) as f64 / after_floored as f64
        };
        let mut s = skew * (after as f64).powf(0.05);
        let non_del = after as f64 / before.max(1) as f64;
        s *= non_del * non_del;
        s
    }

    /// `_forcemerge?max_num_segments=N` (`usize::MAX` = only merge segments with deletions
    /// down, as Lucene's `Integer.MAX_VALUE`). `infos` are the segments to merge (all).
    pub fn find_forced_merges(&self, infos: &[SegInfo], max_segment_count: usize) -> Vec<Vec<usize>> {
        let mut sorted = sorted_by_size(infos);
        let mut total = 0u64;
        let mut running = false;
        sorted.retain(|s| {
            if s.info.merging {
                running = true;
                false
            } else {
                total += s.size;
                true
            }
        });
        let mut max_bytes = self.max_merged_segment_bytes;
        if max_segment_count == 1 {
            max_bytes = u64::MAX;
        } else if max_segment_count != usize::MAX {
            max_bytes = ((total as f64 / max_segment_count as f64) as u64).max(self.max_merged_segment_bytes);
            max_bytes = (max_bytes as f64 * 1.25) as u64;
        }
        let mut found_deletes = false;
        sorted.retain(|s| {
            if s.info.del_count != 0 {
                found_deletes |= s.info.original;
                return true;
            }
            !(max_segment_count == usize::MAX && !s.info.original)
                && !(max_segment_count != usize::MAX && s.size >= max_bytes)
        });
        if sorted.is_empty() {
            return Vec::new();
        }
        if !found_deletes
            && ((max_segment_count != usize::MAX && max_segment_count > 1 && sorted.len() <= max_segment_count)
                || (max_segment_count == 1 && sorted.len() == 1))
        {
            return Vec::new();
        }
        if max_segment_count == usize::MAX && !found_deletes {
            return Vec::new();
        }
        if running {
            return Vec::new();
        }
        if max_segment_count == 1 && total < max_bytes {
            return vec![sorted.iter().map(|s| s.info.id).collect()];
        }
        let mut spec = Vec::new();
        let mut index = sorted.len() as isize - 1;
        let mut resulting = sorted.len();
        loop {
            let mut cand = Vec::new();
            let mut bytes = 0u64;
            while index >= 0 && resulting > max_segment_count {
                let cur = &sorted[index as usize];
                let initial = cand.len();
                if bytes + cur.info.size_bytes <= max_bytes || initial < 2 {
                    cand.push(cur.info.id);
                    index -= 1;
                    bytes += cur.info.size_bytes;
                    if initial > 0 {
                        resulting -= 1;
                    }
                } else {
                    break;
                }
            }
            if cand.len() > 1 {
                spec.push(cand);
            } else if cand.len() == 1 && max_segment_count == usize::MAX {
                // A lone segment with deletions is rewritten to expunge them.
                let id = cand[0];
                if infos.iter().any(|i| i.id == id && i.del_count > 0) {
                    spec.push(cand);
                }
                return spec;
            } else {
                return spec;
            }
        }
    }

    /// `_forcemerge?only_expunge_deletes=true`.
    pub fn find_forced_deletes_merges(&self, infos: &[SegInfo]) -> Vec<Vec<usize>> {
        let mut total_del = 0i64;
        let mut total_max = 0i64;
        let mut work = false;
        for i in infos {
            total_del += i.del_count as i64;
            total_max += i.max_doc as i64;
            let pct = 100.0 * i.del_count as f64 / i.max_doc.max(1) as f64;
            work |= pct > self.expunge_deletes_allowed && !i.merging;
        }
        if !work {
            return Vec::new();
        }
        let mut sorted = sorted_by_size(infos);
        sorted.retain(|s| {
            let pct = 100.0 * s.info.del_count as f64 / s.info.max_doc.max(1) as f64;
            !(s.info.merging || pct <= self.expunge_deletes_allowed)
        });
        let allowed_docs = self.max_allowed_docs(total_max, total_del);
        self.do_find_merges(sorted, usize::MAX, i64::MAX, 0, allowed_docs, MergeType::ForceMergeDeletes, false)
    }

    /// Lucene's `LogMergePolicy.findMerges` by byte size, over segments in index order.
    fn log_find_merges(&self, infos: &[SegInfo]) -> Vec<Vec<usize>> {
        const LEVEL_LOG_SPAN: f64 = 0.75;
        let mf = self.merge_factor.max(2);
        let norm = (mf as f64).ln();
        let min_size = self.floor_segment_bytes.max(1) as f64;
        let max_merge = self.max_merged_segment_bytes;
        let mut levels: Vec<(f64, &SegInfo)> = infos
            .iter()
            .map(|i| {
                let s = (i.size().max(1) as f64).max(min_size);
                (s.ln() / norm, i)
            })
            .collect();
        levels.sort_by_key(|a| a.1.order);
        let mut spec = Vec::new();
        let n = levels.len();
        let mut start = 0;
        while start < n {
            let max_level = levels[start..].iter().map(|l| l.0).fold(f64::MIN, f64::max);
            let level_bottom = if max_level <= (min_size.ln() / norm) {
                -1.0
            } else {
                (max_level - LEVEL_LOG_SPAN).max(min_size.ln() / norm)
            };
            let mut upto = n - 1;
            while upto >= start {
                if levels[upto].0 >= level_bottom {
                    break;
                }
                if upto == 0 {
                    break;
                }
                upto -= 1;
            }
            let mut end = start + mf;
            while end <= 1 + upto {
                let range = &levels[end - mf..end];
                let any_too_large = range.iter().any(|l| l.1.size() >= max_merge / 2);
                let any_merging = range.iter().any(|l| l.1.merging);
                if !any_too_large && !any_merging {
                    spec.push(range.iter().map(|l| l.1.id).collect());
                }
                end += mf;
            }
            start = upto + 1;
        }
        spec
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn segs(sizes: &[u64]) -> Vec<SegInfo> {
        sizes
            .iter()
            .enumerate()
            .map(|(i, &s)| SegInfo { id: i, size_bytes: s, max_doc: (s / 100).max(1) as u32, del_count: 0, merging: false, order: i as u64, original: true })
            .collect()
    }

    #[test]
    fn few_segments_need_no_merge() {
        let p = MergePolicyConfig::default();
        assert!(p.find_merges(&segs(&[1000; 8])).is_empty());
    }

    #[test]
    fn many_small_segments_merge() {
        let p = MergePolicyConfig::default();
        let m = p.find_merges(&segs(&[100_000; 40]));
        assert!(!m.is_empty());
        for g in &m {
            assert!(g.len() >= 2 && g.len() <= 16);
        }
        // No segment in two merges.
        let mut all: Vec<usize> = m.concat();
        all.sort();
        let n = all.len();
        all.dedup();
        assert_eq!(all.len(), n);
    }

    #[test]
    fn deletions_trigger_merges() {
        let p = MergePolicyConfig::default();
        let mut s = segs(&[1 << 20, 1 << 20]);
        s[0].del_count = s[0].max_doc / 2;
        let m = p.find_merges(&s);
        assert!(!m.is_empty());
        let f = p.find_forced_deletes_merges(&s);
        assert!(f.iter().flatten().any(|&i| i == 0));
    }

    #[test]
    fn force_merge_to_one() {
        let p = MergePolicyConfig::default();
        let m = p.find_forced_merges(&segs(&[5, 10, 20, 40]), 1);
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].len(), 4);
        assert!(p.find_forced_merges(&segs(&[5]), 1).is_empty());
        let m = p.find_forced_merges(&segs(&[5, 10, 20, 40]), 2);
        assert!(!m.is_empty());
    }

    #[test]
    fn forced_merges_rewrite_deletes_of_original_segments_only() {
        let p = MergePolicyConfig::default();
        let mut one = segs(&[50]);
        one[0].del_count = 3;
        // An original segment with deletions is rewritten to expunge them.
        assert_eq!(p.find_forced_merges(&one, 1), vec![vec![0]]);
        // The output of the force merge itself (deleted into meanwhile) is done: otherwise
        // concurrent deletes would make the force merge rewrite it forever.
        one[0].original = false;
        assert!(p.find_forced_merges(&one, 1).is_empty());
        // Merged segments still join merges of original ones.
        let mut two = segs(&[50, 5]);
        two[0].original = false;
        assert_eq!(p.find_forced_merges(&two, 1).len(), 1);
        // Expunging (`usize::MAX`) skips a merged segment without deletions.
        let mut three = segs(&[50, 5, 6]);
        three[0].original = false;
        three[1].del_count = 1;
        let m = p.find_forced_merges(&three, usize::MAX);
        assert!(m.iter().all(|g| !g.contains(&0)), "{m:?}");
    }

    #[test]
    fn log_byte_size_merges_adjacent() {
        let p = MergePolicyConfig { merge_factor: 4, ..MergePolicyConfig::time_based() };
        let m = p.find_merges(&segs(&[1000; 9]));
        assert_eq!(m.len(), 2);
        assert_eq!(m[0], vec![0, 1, 2, 3]);
    }
}
