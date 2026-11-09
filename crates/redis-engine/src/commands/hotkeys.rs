//! HOTKEYS: find the keys that cost the most CPU time or network bytes. Like Redis, the top
//! keys are kept in a Cuckoo Heavy Keeper (two cuckoo tables of heavy entries with a lobby per
//! bucket, plus a min-heap of names), sized `count * 10` entries over `count * 100` buckets.

use super::*;
use crate::cmd;
use crate::util::Rng;
use xxhash_rust::xxh3::xxh3_64_with_seed;

pub static COMMANDS: &[Command] = &[Command { since: "8.6.0", ..crate::container!("hotkeys", 0, Server, HOTKEYS_SUBS) }];

static HOTKEYS_SUBS: &[Command] = &[
    cmd!("start", -2, ADMIN | NOSCRIPT, 0, 0, 0, start, Server),
    cmd!("stop", 2, ADMIN | NOSCRIPT, 0, 0, 0, stop, Server),
    cmd!("get", 2, ADMIN | NOSCRIPT, 0, 0, 0, get, Server),
    cmd!("reset", 2, ADMIN | NOSCRIPT, 0, 0, 0, reset, Server),
    cmd!("help", 2, LOADING | STALE, 0, 0, 0, hotkeys_help, Server),
];

const TRACK_CPU: u8 = 1;
const TRACK_NET: u8 = 2;
const METRICS_COUNT: i64 = 2;

/// A tracking session (`server.hotkeys`).
pub struct Stats {
    cpu: Option<TopK>,
    net: Option<TopK>,
    metrics: u8,
    count: usize,
    /// Configured duration while active (0: until stopped); the actual one once stopped.
    duration: i64,
    sample_ratio: i64,
    pub active: bool,
    start: i64,
    ru_start: (i64, i64),
    time_all: u64,
    net_all: u64,
    /// Milliseconds spent updating the top-K structures (`hotkeys-cmd-cpu-time`).
    pub cpu_time: i64,
    rng: Rng,
}

impl Stats {
    fn new(count: usize, duration: i64, sample_ratio: i64, metrics: u8, now: i64) -> Self {
        let buckets = (count * 100).next_power_of_two();
        let topk = || TopK::new(count * 10, buckets, 1.08);
        Self {
            cpu: (metrics & TRACK_CPU != 0).then(topk),
            net: (metrics & TRACK_NET != 0).then(topk),
            metrics,
            count,
            duration,
            sample_ratio,
            active: true,
            start: now,
            ru_start: rusage_ms(),
            time_all: 0,
            net_all: 0,
            cpu_time: 0,
            rng: Rng::new(),
        }
    }

    /// Stop once the configured duration has passed (Redis checks in its cron).
    pub fn expire(&mut self, now: i64) {
        if self.active && self.duration > 0 && now - self.start >= self.duration {
            self.active = false;
            self.duration = now - self.start;
        }
    }

    /// Account one executed command: `micros` of CPU and `bytes` in and out, split evenly over
    /// the keys at `keys`.
    pub fn track(&mut self, args: &[Bytes], keys: &[usize], micros: u64, bytes: u64) {
        if !self.active || keys.is_empty() {
            return;
        }
        let sampled = self.sample_ratio <= 1 || self.rng.unit() < 1.0 / self.sample_ratio as f64;
        let n = keys.len() as u64;
        self.time_all += micros;
        self.net_all += bytes;
        if !sampled {
            return;
        }
        // Redis updates CPU and network in two passes; the order matters for the heap.
        let started = std::time::Instant::now();
        if let Some(t) = self.cpu.as_mut() {
            for &k in keys {
                t.update(&args[k], micros / n);
            }
        }
        self.cpu_time += (started.elapsed().as_micros() / 1000) as i64;
        let started = std::time::Instant::now();
        if let Some(t) = self.net.as_mut() {
            for &k in keys {
                t.update(&args[k], bytes / n);
            }
        }
        self.cpu_time += (started.elapsed().as_micros() / 1000) as i64;
    }

    pub fn memory(&self) -> usize {
        std::mem::size_of::<Self>() + self.cpu.as_ref().map_or(0, TopK::memory) + self.net.as_ref().map_or(0, TopK::memory)
    }
}

/// User and system CPU time of the process, in milliseconds.
// time_t / suseconds_t widths differ between platforms.
#[allow(clippy::unnecessary_cast)]
fn rusage_ms() -> (i64, i64) {
    // SAFETY: getrusage only writes the struct it is given.
    unsafe {
        let mut ru: libc::rusage = std::mem::zeroed();
        libc::getrusage(libc::RUSAGE_SELF, &mut ru);
        let ms = |t: libc::timeval| t.tv_sec as i64 * 1000 + t.tv_usec as i64 / 1000;
        (ms(ru.ru_utime), ms(ru.ru_stime))
    }
}

fn start(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let now = cx.now;
    if let Some(h) = cx.eng.hotkeys.as_mut() {
        h.expire(now);
        if h.active {
            return Err(Error::new("ERR hotkey tracking session already in progress"));
        }
    }
    if args.len() < 4 || !eq(&args[2], "metrics") {
        return Err(Error::new("ERR METRICS parameter is required"));
    }
    // Like getRangeLongFromObjectOrReply: the message covers non-integers too.
    let range = |a: &Bytes, lo: i64, hi: i64, msg: &str| match crate::util::parse_i64(a) {
        Some(v) if (lo..=hi).contains(&v) => Ok(v),
        _ => Err(Error::msg(format!("ERR {msg}"))),
    };
    let wanted = range(&args[3], 1, METRICS_COUNT, &format!("METRICS count must be > 0 and <= {METRICS_COUNT}"))?;
    let (mut metrics, mut parsed, mut valid) = (0u8, 0, 0);
    let mut j = 4;
    while j < args.len() && parsed < wanted {
        for (name, bit) in [("cpu", TRACK_CPU), ("net", TRACK_NET)] {
            if eq(&args[j], name) {
                if metrics & bit != 0 {
                    return Err(Error::msg(format!("ERR METRICS {} defined more than once!", name.to_uppercase())));
                }
                metrics |= bit;
                valid += 1;
            }
        }
        parsed += 1;
        j += 1;
    }
    if parsed != wanted {
        return Err(Error::new("ERR METRICS count does not match number of metric types provided"));
    }
    if valid == 0 {
        return Err(Error::new("ERR METRICS no valid metrics passed. Supported: CPU|NET"));
    }
    let (mut count, mut duration, mut sample) = (10, 0, 1);
    while j < args.len() {
        let more = j + 1 < args.len();
        if more && eq(&args[j], "count") {
            count = range(&args[j + 1], 1, 64, "COUNT must be between 1 and 64")?;
        } else if more && eq(&args[j], "duration") {
            duration = range(&args[j + 1], 1, 1_000_000, "DURATION must be between 1 and 1000000")? * 1000;
        } else if more && eq(&args[j], "sample") {
            sample = range(&args[j + 1], 1, i32::MAX as i64, "SAMPLE ratio must be positive")?;
        } else if more && eq(&args[j], "slots") {
            return Err(Error::new("ERR SLOTS parameter cannot be used in non-cluster mode"));
        } else {
            return Err(SYNTAX);
        }
        j += 2;
    }
    cx.eng.hotkeys = Some(Stats::new(count as usize, duration, sample, metrics, now));
    Ok(Reply::Ok)
}

fn stop(cx: &mut Cx, _args: &[Bytes]) -> CmdResult {
    let now = cx.now;
    match cx.eng.hotkeys.as_mut() {
        Some(h) => {
            h.expire(now);
            if !h.active {
                return Ok(Reply::Nil);
            }
            h.active = false;
            h.duration = now - h.start;
            Ok(Reply::Ok)
        }
        None => Ok(Reply::Nil),
    }
}

fn get(cx: &mut Cx, _args: &[Bytes]) -> CmdResult {
    let now = cx.now;
    let Some(h) = cx.eng.hotkeys.as_mut() else { return Ok(Reply::Nil) };
    h.expire(now);
    let duration = if h.active { now - h.start } else { h.duration };
    let kv = |k: &str, v: Reply| (Reply::bulk(k), v);
    let int = |v: i64| Reply::Int(v);
    let mut m = vec![
        kv("tracking-active", int(h.active as i64)),
        kv("sample-ratio", int(h.sample_ratio)),
        // Without cluster mode the node serves every slot.
        kv("selected-slots", Reply::Array(vec![Reply::Array(vec![int(0), int(16383)])])),
        kv("all-commands-all-slots-us", int(h.time_all as i64)),
        kv("net-bytes-all-commands-all-slots", int(h.net_all as i64)),
        kv("collection-start-time-unix-ms", int(h.start)),
        kv("collection-duration-ms", int(duration)),
    ];
    if h.metrics & TRACK_CPU != 0 {
        let (user, sys) = rusage_ms();
        m.push(kv("total-cpu-time-user-ms", int(user - h.ru_start.0)));
        m.push(kv("total-cpu-time-sys-ms", int(sys - h.ru_start.1)));
    }
    if let Some(t) = &h.net {
        m.push(kv("total-net-bytes", int(t.total as i64)));
    }
    let list = |t: &TopK| {
        let mut out = Vec::new();
        for (item, count) in t.list().into_iter().take(h.count) {
            out.push(Reply::Bulk(item));
            out.push(int(count as i64));
        }
        Reply::Array(out)
    };
    if let Some(t) = &h.cpu {
        m.push(kv("by-cpu-time-us", list(t)));
    }
    if let Some(t) = &h.net {
        m.push(kv("by-net-bytes", list(t)));
    }
    Ok(Reply::Array(vec![Reply::Map(m)]))
}

fn reset(cx: &mut Cx, _args: &[Bytes]) -> CmdResult {
    let now = cx.now;
    if let Some(h) = cx.eng.hotkeys.as_mut() {
        h.expire(now);
        if h.active {
            return Err(Error::new("ERR hotkey tracking session in progress, stop tracking first"));
        }
    }
    cx.eng.hotkeys = None;
    Ok(Reply::Ok)
}

fn hotkeys_help(_cx: &mut Cx, _args: &[Bytes]) -> CmdResult {
    Ok(help(&[
        "HOTKEYS <subcommand> [<arg> [value] [opt] ...]. Subcommands are:",
        "START <METRICS count [CPU] [NET]> [COUNT k] [DURATION duration] [SAMPLE ratio] [SLOTS count slot...]",
        "    Starts hotkeys tracking with specified metrics.",
        "    * METRICS count [CPU] [NET]",
        "        Specify count of metrics and choose amongst:",
        "        - CPU: Track hotkeys by CPU time percentage",
        "        - NET: Track hotkeys by network bytes percentage",
        "    * COUNT k",
        "        Specifies the value of K for the top-K hotkeys tracking. Default: 10",
        "    * DURATION duration",
        "        Specifies tracking duration in seconds. 0 means tracking will continue until manually stopped. Default: 0",
        "    * SAMPLE ratio",
        "        Keys are tracked with probability 1/ratio. Default: 1 (tracks every key)",
        "    * SLOTS count slot...",
        "        Specify which slots to track keys from. Only available in cluster mode. Default: empty (track all slots)",
        "STOP",
        "    Stop hotkeys tracking. Results are still available via GET",
        "GET",
        "    Get results from hotkeys tracking.",
        "RESET",
        "    Reset memory used for hotkeys tracking. Tracking must have been stopped.",
        "    Results will no longer be available after this command.",
        "HELP",
        "    Print this help.",
    ]))
}

// ---- Cuckoo Heavy Keeper ----

const LUT_SIZE: usize = 256;
const HEAVY_PER_BUCKET: usize = 2;
/// Lobby counters past this are promoted to heavy entries.
const PROMOTION: u64 = 16;
/// Bound on the chain of heavy entries a promotion may displace.
const MAX_KICKS: usize = 16;
/// An item is a heavy hitter when its count is at least this share of the total.
const HEAVY_RATIO: f64 = 0.008;
/// Heap names are identified with a hash unrelated to the tables'.
const HEAP_SEED: u64 = 1919;

#[derive(Clone, Copy, Default)]
struct Heavy {
    count: u64,
    fp: u16,
}

#[derive(Clone, Copy, Default)]
struct Lobby {
    fp: u16,
    count: u8,
}

#[derive(Clone, Copy, Default)]
struct Bucket {
    heavy: [Heavy; HEAVY_PER_BUCKET],
    lobby: Lobby,
}

#[derive(Clone, Default)]
struct HeapItem {
    count: u64,
    item: Option<Bytes>,
    fp: u64,
}

struct TopK {
    tables: [Vec<Bucket>; 2],
    heap: Vec<HeapItem>,
    lut_exp: [f64; LUT_SIZE + 1],
    lut_min: [f64; LUT_SIZE + 1],
    lut_prob: [f64; LUT_SIZE + 1],
    total: u64,
    mask: usize,
    rng: Rng,
}

struct Hashed {
    idx: [usize; 2],
    fp: u16,
}

fn alt_idx(fp: u16, idx: usize, mask: usize) -> usize {
    (idx ^ 0x5bd1_e995usize.wrapping_mul(fp as usize)) & mask
}

impl TopK {
    fn new(k: usize, buckets: usize, decay: f64) -> Self {
        let (mut lut_exp, mut lut_min, mut lut_prob) = ([0.0; LUT_SIZE + 1], [0.0; LUT_SIZE + 1], [0.0; LUT_SIZE + 1]);
        for i in 1..=LUT_SIZE {
            lut_exp[i] = lut_exp[i - 1] + decay.powi(i as i32 - 1);
            lut_min[i] = lut_exp[i] - lut_exp[i - 1];
            lut_prob[i] = (1.0 / decay).powi(i as i32);
        }
        Self {
            tables: [vec![Bucket::default(); buckets], vec![Bucket::default(); buckets]],
            heap: vec![HeapItem::default(); k],
            lut_exp,
            lut_min,
            lut_prob,
            total: 0,
            mask: buckets - 1,
            rng: Rng::new(),
        }
    }

    fn memory(&self) -> usize {
        std::mem::size_of::<Self>()
            + 2 * self.tables[0].len() * std::mem::size_of::<Bucket>()
            + self.heap.len() * std::mem::size_of::<HeapItem>()
            + self.heap.iter().map(|h| h.item.as_ref().map_or(0, |b| b.len())).sum::<usize>()
    }

    fn hash(&self, item: &[u8]) -> Hashed {
        let h = xxh3_64_with_seed(item, 0);
        let fp = (h & 0xffff) as u16;
        let i0 = ((h >> 32) as usize) & self.mask;
        Hashed { idx: [i0, alt_idx(fp, i0, self.mask)], fp }
    }

    fn heavy_hitter(&self, count: u64) -> bool {
        count as f64 >= self.total as f64 * HEAVY_RATIO
    }

    /// Bump a heavy entry, or take an empty heavy slot directly (skipping the lobby).
    fn check_heavy(&mut self, it: &Hashed, weight: u64) -> Option<(usize, usize)> {
        let mut empty = None;
        for t in 0..2 {
            for j in 0..HEAVY_PER_BUCKET {
                let e = &mut self.tables[t][it.idx[t]].heavy[j];
                if e.count > 0 {
                    if e.fp == it.fp {
                        e.count += weight;
                        return Some((t, j));
                    }
                } else if empty.is_none() {
                    empty = Some((t, j));
                }
            }
        }
        let (t, j) = empty?;
        self.tables[t][it.idx[t]].heavy[j] = Heavy { count: weight, fp: it.fp };
        Some((t, j))
    }

    /// A displaced heavy entry moves to its alternate bucket, displacing the smallest there.
    fn kickout(&mut self, mut entry: Heavy, mut idx: usize, mut t: usize) {
        for _ in 0..MAX_KICKS {
            if !self.heavy_hitter(entry.count) {
                return;
            }
            t = 1 - t;
            idx = alt_idx(entry.fp, idx, self.mask);
            let bucket = &mut self.tables[t][idx];
            let (mut min, mut min_pos) = (u64::MAX, 0);
            for j in 0..HEAVY_PER_BUCKET {
                let e = &mut bucket.heavy[j];
                if e.count == 0 {
                    *e = entry;
                    return;
                }
                if e.count < min {
                    min = e.count;
                    min_pos = j;
                }
            }
            entry = std::mem::replace(&mut bucket.heavy[min_pos], entry);
        }
    }

    /// Try to promote a lobby entry of table `t` that reached `count`; the heavy position on success.
    fn promote(&mut self, it: &Hashed, count: u64, t: usize) -> Option<usize> {
        let idx = it.idx[t];
        let (mut min, mut min_pos) = (u64::MAX, 0);
        for j in 0..HEAVY_PER_BUCKET {
            let e = &mut self.tables[t][idx].heavy[j];
            if e.count == 0 {
                *e = Heavy { count, fp: it.fp };
                return Some(j);
            }
            if e.count < min {
                min = e.count;
                min_pos = j;
            }
        }
        if min > count {
            let prob = (count - PROMOTION) as f64 / (min - PROMOTION) as f64;
            if self.rng.unit() >= prob {
                return None;
            }
        }
        let bucket = &mut self.tables[t][idx];
        let out = bucket.heavy[min_pos];
        // The promoted item keeps the displaced entry's count, as in the paper.
        bucket.heavy[min_pos].fp = bucket.lobby.fp;
        bucket.lobby = Lobby::default();
        self.kickout(out, idx, t);
        Some(min_pos)
    }

    fn check_lobby(&mut self, it: &Hashed, weight: u64) -> Option<(usize, Option<usize>)> {
        for t in 0..2 {
            let e = self.tables[t][it.idx[t]].lobby;
            if e.fp != it.fp || e.count == 0 {
                continue;
            }
            let count = e.count as u64 + weight;
            if count < PROMOTION {
                self.tables[t][it.idx[t]].lobby.count = count as u8;
                return Some((t, None));
            }
            if let Some(pos) = self.promote(it, count, t) {
                return Some((t, Some(pos)));
            }
            self.tables[t][it.idx[t]].lobby.count = PROMOTION as u8;
            return Some((t, None));
        }
        None
    }

    fn decay_prob(&self, cnt: u64) -> f64 {
        let cnt = cnt as usize;
        if cnt < LUT_SIZE {
            return self.lut_prob[cnt];
        }
        self.lut_prob[LUT_SIZE].powf(cnt as f64 / LUT_SIZE as f64) * self.lut_prob[cnt % LUT_SIZE]
    }

    fn min_decay(&self, cnt: u64) -> f64 {
        let cnt = cnt as usize;
        if cnt < LUT_SIZE {
            return self.lut_min[cnt];
        }
        self.lut_min[LUT_SIZE].powf(cnt as f64 / LUT_SIZE as f64) * self.lut_min[cnt % LUT_SIZE]
    }

    /// Decay a colliding lobby counter by the weight of the newcomer.
    fn decay(&mut self, cnt: u8, weight: u64) -> u8 {
        if weight == 0 {
            return cnt;
        }
        if weight == 1 {
            return if self.rng.unit() < self.decay_prob(cnt as u64) { cnt - 1 } else { cnt };
        }
        let min = self.min_decay(cnt as u64);
        if weight < min as u64 {
            return if self.rng.unit() < weight as f64 / min { cnt - 1 } else { cnt };
        }
        if weight >= self.lut_exp[cnt as usize] as u64 {
            return 0;
        }
        let (mut lo, mut hi) = (0usize, cnt as usize);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if self.lut_exp[mid] + weight as f64 >= self.lut_exp[cnt as usize] {
                hi = mid;
            } else {
                lo = mid + 1;
            }
        }
        lo as u8
    }

    fn update(&mut self, item: &[u8], weight: u64) {
        if weight == 0 {
            return;
        }
        self.total += weight;
        let it = self.hash(item);
        let at = if let Some(at) = self.check_heavy(&it, weight) {
            Some(at)
        } else if let Some((t, pos)) = self.check_lobby(&it, weight) {
            pos.map(|p| (t, p))
        } else {
            self.place(&it, weight)
        };
        if let Some((t, pos)) = at {
            self.update_heap(item, self.tables[t][it.idx[t]].heavy[pos].count);
        }
    }

    /// An untracked item: take an empty lobby, or decay the one its fingerprint picks.
    fn place(&mut self, it: &Hashed, weight: u64) -> Option<(usize, usize)> {
        for t in 0..2 {
            if self.tables[t][it.idx[t]].lobby.count == 0 {
                self.tables[t][it.idx[t]].lobby.fp = it.fp;
                if weight < PROMOTION {
                    self.tables[t][it.idx[t]].lobby.count = weight as u8;
                    return None;
                }
                let pos = self.promote(it, weight, t);
                if pos.is_none() {
                    self.tables[t][it.idx[t]].lobby.count = PROMOTION as u8;
                }
                return pos.map(|p| (t, p));
            }
        }
        let t = (it.fp & 1) as usize;
        let idx = it.idx[t];
        let old = self.tables[t][idx].lobby.count;
        let new = self.decay(old, weight);
        let e = &mut self.tables[t][idx].lobby;
        if new == 0 {
            e.fp = it.fp;
            let exp = self.lut_exp[old as usize] as u64;
            e.count = if exp >= weight { 1 } else { (weight - exp).min(255) as u8 };
        } else {
            e.count = new;
        }
        let count = e.count as u64;
        if count >= PROMOTION {
            return self.promote(it, count, t).map(|p| (t, p));
        }
        None
    }

    fn update_heap(&mut self, item: &[u8], count: u64) {
        if count < self.heap[0].count {
            return;
        }
        let fp = xxh3_64_with_seed(item, HEAP_SEED);
        let found = (0..self.heap.len())
            .rev()
            .find(|&i| self.heap[i].fp == fp && self.heap[i].item.as_deref() == Some(item));
        match found {
            Some(i) => {
                self.heap[i].count = count;
                heapify_down(&mut self.heap, i);
            }
            None => {
                self.heap[0] = HeapItem { count, item: Some(Bytes::copy_from_slice(item)), fp };
                heapify_down(&mut self.heap, 0);
            }
        }
    }

    /// The heap's items by descending count (empty slots last).
    fn list(&self) -> Vec<(Bytes, u64)> {
        let mut v: Vec<&HeapItem> = self.heap.iter().collect();
        v.sort_by_key(|h| std::cmp::Reverse(h.count));
        v.into_iter().take_while(|h| h.count > 0).filter_map(|h| Some((h.item.clone()?, h.count))).collect()
    }
}

fn heapify_down(a: &mut [HeapItem], mut start: usize) {
    let len = a.len();
    if len < 2 || (len - 2) / 2 < start {
        return;
    }
    let mut child = 2 * start + 1;
    if child + 1 < len && a[child].count > a[child + 1].count {
        child += 1;
    }
    if a[child].count > a[start].count {
        return;
    }
    let top = std::mem::take(&mut a[start]);
    loop {
        a[start] = std::mem::take(&mut a[child]);
        start = child;
        if (len - 2) / 2 < child {
            break;
        }
        child = 2 * child + 1;
        if child + 1 < len && a[child].count > a[child + 1].count {
            child += 1;
        }
        if a[child].count >= top.count {
            break;
        }
    }
    a[start] = top;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn topk_finds_the_heavy_keys() {
        let mut t = TopK::new(100, 1024, 1.08);
        let mut rng = Rng::seeded(7);
        for i in 0..20_000u64 {
            // key0 gets 5000 hits, key1 3000, key2 1000, and noise over 2000 keys.
            let k = match i % 20 {
                0..=4 => "key0".to_string(),
                5..=7 => "key1".to_string(),
                8 => "key2".to_string(),
                _ => format!("noise{}", rng.below(2000)),
            };
            t.update(k.as_bytes(), 1);
        }
        let top: Vec<_> = t.list().into_iter().take(3).map(|(k, c)| (String::from_utf8(k.to_vec()).unwrap(), c)).collect();
        assert_eq!(top.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(), ["key0", "key1", "key2"]);
        assert_eq!(top[0].1, 5000);
    }

    #[test]
    fn weights_are_exact_for_few_keys() {
        let mut t = TopK::new(10, 128, 1.08);
        for (k, w) in [("a", 30), ("b", 20), ("a", 5), ("c", 1)] {
            t.update(k.as_bytes(), w);
        }
        let got: Vec<_> = t.list().into_iter().map(|(k, c)| (k.to_vec(), c)).collect();
        assert_eq!(got, vec![(b"a".to_vec(), 35), (b"b".to_vec(), 20), (b"c".to_vec(), 1)]);
    }
}
