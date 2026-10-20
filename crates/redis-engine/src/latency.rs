//! The latency monitor (Redis' `latency.c`): per-event time series of the spikes that reached
//! `latency-monitor-threshold` (one sample per second, the worst kept), and the LATENCY
//! LATEST/HISTORY/GRAPH/DOCTOR/RESET views of them, with the sparkline renderer GRAPH uses
//! (`sparkline.c`).

/// `LATENCY_TS_LEN`: samples kept per event.
const TS_LEN: usize = 160;
/// `LATENCY_GRAPH_COLS`.
const GRAPH_COLS: usize = 80;

#[derive(Clone, Copy, Debug, Default)]
struct Sample {
    /// Unix seconds; 0 for an unused slot.
    time: i32,
    /// Milliseconds.
    latency: u32,
}

/// `latencyTimeSeries`.
#[derive(Clone, Debug)]
pub struct Series {
    idx: usize,
    max: u32,
    samples: [Sample; TS_LEN],
}

impl Default for Series {
    fn default() -> Self {
        Series { idx: 0, max: 0, samples: [Sample::default(); TS_LEN] }
    }
}

impl Series {
    /// The samples oldest first (the ring from the next slot to write).
    fn in_order(&self) -> impl Iterator<Item = &Sample> {
        (0..TS_LEN).map(|j| &self.samples[(self.idx + j) % TS_LEN]).filter(|s| s.time != 0)
    }
}

/// `server.latency_events`: the series, and the dict holding them (its order is LATENCY
/// LATEST's and DOCTOR's).
#[derive(Clone, Debug, Default)]
pub struct Monitor {
    events: Vec<(String, Box<Series>)>,
    dict: crate::sdict::ServerDict,
}

/// What `createLatencyReport` reads from the rest of the server.
pub struct ReportCtx {
    pub threshold_ms: u64,
    pub slowlog_slower_than: i64,
    pub slowlog_max_len: u64,
    pub fsync_always: bool,
    pub hz: u32,
    pub now: i64,
    /// `THPGetAnonHugePagesSize() > 0`: the process has transparent huge pages.
    pub anon_huge_pages: bool,
}

/// `zmalloc_get_smap_bytes_by_field("AnonHugePages:", -1)`: the field summed over
/// `/proc/self/smaps` (kB to bytes); 0 where there is no such file.
pub fn anon_huge_pages_bytes() -> u64 {
    let Ok(smaps) = std::fs::read_to_string("/proc/self/smaps") else { return 0 };
    smaps
        .lines()
        .filter_map(|l| l.strip_prefix("AnonHugePages:"))
        .filter_map(|rest| rest.split_once('k').map(|(n, _)| n.trim().parse::<u64>().unwrap_or(0) * 1024))
        .sum()
}

impl Monitor {
    /// `dictFetchValue` (a rehash step).
    fn get(&mut self, event: &str) -> Option<&Series> {
        self.dict.find(event.as_bytes())?;
        self.events.iter().find(|(e, _)| e == event).map(|(_, s)| &**s)
    }

    /// The events in the dict's iteration order.
    fn ordered(&self) -> impl Iterator<Item = &(String, Box<Series>)> {
        self.dict.iter().filter_map(|k| self.events.iter().find(|(e, _)| e.as_bytes() == k))
    }

    /// `latencyAddSampleIfNeeded`: record `ms` for `event` when the monitor is on and the
    /// latency reached its threshold.
    pub fn add_if_needed(&mut self, threshold_ms: u64, event: &str, ms: u64, now_secs: i64) {
        if threshold_ms != 0 && ms >= threshold_ms {
            self.add(event, ms, now_secs);
        }
    }

    /// `latencyAddSample`.
    pub fn add(&mut self, event: &str, ms: u64, now_secs: i64) {
        let latency = ms.min(u32::MAX as u64) as u32;
        let now = now_secs as i32;
        let found = self.dict.find(event.as_bytes()).is_some();
        let ts = match self.events.iter().position(|(e, _)| e == event).filter(|_| found) {
            Some(i) => &mut self.events[i].1,
            None => {
                self.dict.add(event.as_bytes());
                self.events.push((event.to_string(), Box::default()));
                &mut self.events.last_mut().unwrap().1
            }
        };
        if latency > ts.max {
            ts.max = latency;
        }
        // A sample in the same second keeps the worst latency.
        let prev = (ts.idx + TS_LEN - 1) % TS_LEN;
        if ts.samples[prev].time == now {
            if latency > ts.samples[prev].latency {
                ts.samples[prev].latency = latency;
            }
            return;
        }
        ts.samples[ts.idx] = Sample { time: now, latency };
        ts.idx = (ts.idx + 1) % TS_LEN;
    }

    /// `latencyResetEvent`: drop one event (case-insensitively) or all; how many were dropped.
    pub fn reset(&mut self, event: Option<&[u8]>) -> usize {
        let pick = |name: &[u8]| event.is_none_or(|e| name.eq_ignore_ascii_case(e));
        self.events.retain(|(name, _)| !pick(name.as_bytes()));
        self.dict.delete_while_iterating(pick)
    }

    /// LATENCY LATEST: `(event, last time, last latency, all-time max)` per event.
    pub fn latest(&self) -> Vec<(&str, i64, u64, u64)> {
        self.ordered()
            .map(|(e, ts)| {
                let last = ts.samples[(ts.idx + TS_LEN - 1) % TS_LEN];
                (e.as_str(), last.time as i64, last.latency as u64, ts.max as u64)
            })
            .collect()
    }

    /// LATENCY HISTORY: the event's `(time, latency)` samples, oldest first (none: empty).
    pub fn history(&mut self, event: &[u8]) -> Vec<(i64, u64)> {
        let Some(ts) = std::str::from_utf8(event).ok().and_then(|e| self.get(e)) else { return Vec::new() };
        ts.in_order().map(|s| (s.time as i64, s.latency as u64)).collect()
    }

    /// LATENCY GRAPH (`latencyCommandGenSparkeline`); `None` when the event has no series.
    pub fn graph(&mut self, event: &[u8], now: i64) -> Option<String> {
        let name = std::str::from_utf8(event).ok()?;
        let ts = self.get(name)?;
        let mut seq = Sequence::default();
        let (mut min, mut max) = (0u32, 0u32);
        for s in ts.in_order() {
            if seq.samples.is_empty() {
                min = s.latency;
                max = s.latency;
            } else {
                max = max.max(s.latency);
                min = min.min(s.latency);
            }
            let elapsed = (now - s.time as i64) as i32;
            let label = if elapsed < 60 {
                format!("{elapsed}s")
            } else if elapsed < 3600 {
                format!("{}m", elapsed / 60)
            } else if elapsed < 3600 * 24 {
                format!("{}h", elapsed / 3600)
            } else {
                format!("{}d", elapsed / (3600 * 24))
            };
            seq.add(s.latency as f64, Some(label));
        }
        let mut g = format!("{name} - high {max} ms, low {min} ms (all time high {} ms)\n", ts.max);
        g.push_str(&"-".repeat(GRAPH_COLS));
        g.push('\n');
        seq.render(&mut g, GRAPH_COLS, 4, true);
        Some(g)
    }

    /// `analyzeLatencyForEvent`: (samples, avg, mad, period).
    fn analyze(ts: &Series, now: i64) -> (u32, u64, u64, i64) {
        let (mut n, mut sum, mut oldest) = (0u32, 0u64, 0i64);
        for s in ts.samples.iter().filter(|s| s.time != 0) {
            n += 1;
            sum += s.latency as u64;
            if oldest == 0 || (s.time as i64) < oldest {
                oldest = s.time as i64;
            }
        }
        if n == 0 {
            return (0, 0, 0, 0);
        }
        let avg = sum / n as u64;
        let period = (now - oldest).max(0);
        let period = if period == 0 { 1 } else { period };
        let dev: u64 = ts.samples.iter().filter(|s| s.time != 0).map(|s| (avg as i64 - s.latency as i64).unsigned_abs()).sum();
        (n, avg, dev / n as u64, period)
    }

    /// LATENCY DOCTOR (`createLatencyReport`). Events Redis records but roll never produces
    /// (fork, AOF fsync and rename, …) keep their advice for completeness.
    pub fn doctor(&self, cx: &ReportCtx) -> String {
        if self.events.is_empty() && cx.threshold_ms == 0 {
            return "I'm sorry, Dave, I can't do that. Latency monitoring is disabled in this Redis instance. You may use \"CONFIG SET latency-monitor-threshold <milliseconds>.\" in order to enable it. If we weren't in a deep space mission I'd suggest to take a look at https://redis.io/docs/latest/operate/oss_and_stack/management/optimization/latency-monitor.\n".to_string();
        }
        #[derive(Default)]
        struct Advice {
            slowlog_enabled: bool,
            slowlog_tuning: bool,
            slowlog_inspect: bool,
            disk_contention: bool,
            scheduler: bool,
            data_writeback: bool,
            no_appendfsync: bool,
            local_disk: bool,
            ssd: bool,
            write_load_info: bool,
            hz: bool,
            large_objects: bool,
            mass_eviction: bool,
            relax_fsync_policy: bool,
        }
        let mut a = Advice::default();
        let mut advices = 0;
        let mut report = String::new();
        for (num, (event, ts)) in self.ordered().enumerate() {
            if num == 0 {
                report.push_str("Dave, I have observed latency spikes in this Redis instance. You don't mind talking about it, do you Dave?\n\n");
            }
            let (n, avg, mad, period) = Self::analyze(ts, cx.now);
            report.push_str(&format!(
                "{}. {}: {} latency spikes (average {}ms, mean deviation {}ms, period {:.2} sec). Worst all time event {}ms.",
                num + 1,
                event,
                n,
                avg,
                mad,
                period as f64 / n as f64,
                ts.max
            ));
            let ev = event.to_ascii_lowercase();
            match ev.as_str() {
                "command" => {
                    if cx.slowlog_slower_than < 0 || cx.slowlog_max_len == 0 {
                        a.slowlog_enabled = true;
                        advices += 1;
                    } else if (cx.slowlog_slower_than / 1000) as u64 > cx.threshold_ms {
                        a.slowlog_tuning = true;
                        advices += 1;
                    }
                    a.slowlog_inspect = true;
                    a.large_objects = true;
                    advices += 2;
                }
                "fast-command" => {
                    a.scheduler = true;
                    advices += 1;
                }
                "aof-write-pending-fsync" => {
                    a.local_disk = true;
                    a.disk_contention = true;
                    a.ssd = true;
                    a.data_writeback = true;
                    advices += 4;
                }
                "aof-write-active-child" => {
                    a.no_appendfsync = true;
                    a.data_writeback = true;
                    a.ssd = true;
                    advices += 3;
                }
                "aof-write-alone" => {
                    a.local_disk = true;
                    a.data_writeback = true;
                    a.ssd = true;
                    advices += 3;
                }
                "aof-fsync-always" => {
                    a.relax_fsync_policy = true;
                    advices += 1;
                }
                "aof-fstat" | "rdb-unlink-temp-file" => {
                    a.disk_contention = true;
                    a.local_disk = true;
                    advices += 2;
                }
                "aof-rewrite-diff-write" | "aof-rename" => {
                    a.write_load_info = true;
                    a.data_writeback = true;
                    a.ssd = true;
                    a.local_disk = true;
                    advices += 4;
                }
                "expire-cycle" => {
                    a.hz = true;
                    a.large_objects = true;
                    advices += 2;
                }
                "eviction-del" => {
                    a.large_objects = true;
                    advices += 1;
                }
                "eviction-cycle" => {
                    a.mass_eviction = true;
                    advices += 1;
                }
                _ => {}
            }
            report.push('\n');
        }
        let eventnum = self.events.len();
        let thp = cx.anon_huge_pages;
        if thp {
            advices += 1;
        }
        if eventnum == 0 && advices == 0 {
            report.push_str("Dave, no latency spike was observed during the lifetime of this Redis instance, not in the slightest bit. I honestly think you ought to sit down calmly, take a stress pill, and think things over.\n");
            return report;
        }
        if advices == 0 {
            report.push_str("\nWhile there are latency events logged, I'm not able to suggest any easy fix. Please use the Redis community to get some help, providing this report in your help request.\n");
            return report;
        }
        report.push_str("\nI have a few advices for you:\n\n");
        if a.slowlog_enabled {
            report.push_str(&format!("- There are latency issues with potentially slow commands you are using. Try to enable the Slow Log Redis feature using the command 'CONFIG SET slowlog-log-slower-than {}'. If the Slow log is disabled Redis is not able to log slow commands execution for you.\n", cx.threshold_ms * 1000));
        }
        if a.slowlog_tuning {
            report.push_str(&format!("- Your current Slow Log configuration only logs events that are slower than your configured latency monitor threshold. Please use 'CONFIG SET slowlog-log-slower-than {}'.\n", cx.threshold_ms * 1000));
        }
        if a.slowlog_inspect {
            report.push_str("- Check your Slow Log to understand what are the commands you are running which are too slow to execute. Please check https://redis.io/commands/slowlog for more information.\n");
        }
        if a.scheduler {
            report.push_str("- The system is slow to execute Redis code paths not containing system calls. This usually means the system does not provide Redis CPU time to run for long periods. You should try to:\n  1) Lower the system load.\n  2) Use a computer / VM just for Redis if you are running other software in the same system.\n  3) Check if you have a \"noisy neighbour\" problem.\n  4) Check with 'redis-cli --intrinsic-latency 100' what is the intrinsic latency in your system.\n  5) Check if the problem is allocator-related by recompiling Redis with MALLOC=libc, if you are using Jemalloc. However this may create fragmentation problems.\n");
        }
        if a.local_disk {
            report.push_str("- It is strongly advised to use local disks for persistence, especially if you are using AOF. Remote disks provided by platform-as-a-service providers are known to be slow.\n");
        }
        if a.ssd {
            report.push_str("- SSD disks are able to reduce fsync latency, and total time needed for snapshotting and AOF log rewriting (resulting in smaller memory usage). With extremely high write load SSD disks can be a good option. However Redis should perform reasonably with high load using normal disks. Use this advice as a last resort.\n");
        }
        if a.data_writeback {
            report.push_str("- Mounting ext3/4 filesystems with data=writeback can provide a performance boost compared to data=ordered, however this mode of operation provides less guarantees, and sometimes it can happen that after a hard crash the AOF file will have a half-written command at the end and will require to be repaired before Redis restarts.\n");
        }
        if a.disk_contention {
            report.push_str("- Try to lower the disk contention. This is often caused by other disk intensive processes running in the same computer (including other Redis instances).\n");
        }
        if a.no_appendfsync {
            report.push_str("- Assuming from the point of view of data safety this is viable in your environment, you could try to enable the 'no-appendfsync-on-rewrite' option, so that fsync will not be performed while there is a child rewriting the AOF file or producing an RDB file (the moment where there is high disk contention).\n");
        }
        if a.relax_fsync_policy && cx.fsync_always {
            report.push_str("- Your fsync policy is set to 'always'. It is very hard to get good performances with such a setup, if possible try to relax the fsync policy to 'onesec'.\n");
        }
        if a.write_load_info {
            report.push_str("- Latency during the AOF atomic rename operation or when the final difference is flushed to the AOF file at the end of the rewrite, sometimes is caused by very high write load, causing the AOF buffer to get very large. If possible try to send less commands to accomplish the same work, or use Lua scripts to group multiple operations into a single EVALSHA call.\n");
        }
        if a.hz && cx.hz < 100 {
            report.push_str("- In order to make the Redis keys expiring process more incremental, try to set the 'hz' configuration parameter to 100 using 'CONFIG SET hz 100'.\n");
        }
        if a.large_objects {
            report.push_str("- Deleting, expiring or evicting (because of maxmemory policy) large objects is a blocking operation. If you have very large objects that are often deleted, expired, or evicted, try to fragment those objects into multiple smaller objects.\n");
        }
        if a.mass_eviction {
            report.push_str("- Sudden changes to the 'maxmemory' setting via 'CONFIG SET', or allocation of large objects via sets or sorted sets intersections, STORE option of SORT, Redis Cluster large keys migrations (RESTORE command), may create sudden memory pressure forcing the server to block trying to evict keys. \n");
        }
        if thp {
            report.push_str("- I detected a non zero amount of anonymous huge pages used by your process. This creates very serious latency events in different conditions, especially when Redis is persisting on disk. To disable THP support use the command 'echo never > /sys/kernel/mm/transparent_hugepage/enabled', make sure to also add it into /etc/rc.local so that the command will be executed again after a reboot. Note that even if you have already disabled THP, you still need to restart the Redis process to get rid of the huge pages already created.\n");
        }
        report
    }
}

/// `sparkline.c`'s sequence.
#[derive(Default)]
struct Sequence {
    samples: Vec<(f64, Option<String>)>,
    labels: usize,
    min: f64,
    max: f64,
}

const CHARSET: &[u8] = b"_-`";
const CHARSET_FILL: &[u8] = b"_o#";
const LABEL_MARGIN_TOP: usize = 1;

impl Sequence {
    /// `sparklineSequenceAddSample`.
    fn add(&mut self, value: f64, label: Option<String>) {
        let label = label.filter(|l| !l.is_empty());
        if self.samples.is_empty() {
            self.min = value;
            self.max = value;
        } else if value < self.min {
            self.min = value;
        } else if value > self.max {
            self.max = value;
        }
        if label.is_some() {
            self.labels += 1;
        }
        self.samples.push((value, label));
    }

    /// `sparklineRender` (linear scale).
    fn render(&self, out: &mut String, columns: usize, rows: usize, fill: bool) {
        let mut j = 0;
        while j < self.samples.len() {
            let len = (self.samples.len() - j).min(columns);
            if j != 0 {
                out.push('\n');
            }
            self.render_range(out, rows, j, len, fill);
            j += columns;
        }
    }

    /// `sparklineRenderRange`.
    fn render_range(&self, out: &mut String, rows: usize, offset: usize, len: usize, fill: bool) {
        let cl = CHARSET.len() as i32;
        let mut relmax = self.max - self.min;
        if relmax == 0.0 {
            relmax = 1.0;
        }
        let steps = cl * rows as i32;
        let mut row = 0usize;
        let mut chars = vec![b' '; len];
        loop {
            let mut again = false;
            chars.iter_mut().for_each(|c| *c = b' ');
            for (j, ch) in chars.iter_mut().enumerate() {
                let (value, label) = &self.samples[j + offset];
                let relval = value - self.min;
                let mut step = ((relval * steps as f64) / relmax) as i32;
                step = step.clamp(0, steps - 1);
                if row < rows {
                    let charidx = step - ((rows - row - 1) as i32 * cl);
                    again = true;
                    if (0..cl).contains(&charidx) {
                        *ch = if fill { CHARSET_FILL[charidx as usize] } else { CHARSET[charidx as usize] };
                    } else if fill && charidx >= cl {
                        *ch = b'|';
                    }
                } else {
                    if self.labels > 0 && row - rows < LABEL_MARGIN_TOP {
                        again = true;
                        break;
                    }
                    if let Some(l) = label {
                        let label_char = row - rows - LABEL_MARGIN_TOP;
                        if l.len() > label_char {
                            again = true;
                            *ch = l.as_bytes()[label_char];
                        }
                    }
                }
            }
            if !again {
                break;
            }
            row += 1;
            out.push_str(&String::from_utf8_lossy(&chars));
            out.push('\n');
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn samples_per_second_keep_the_worst() {
        let mut m = Monitor::default();
        m.add_if_needed(0, "command", 500, 100);
        assert!(m.latest().is_empty());
        m.add_if_needed(10, "command", 5, 100);
        assert!(m.latest().is_empty());
        m.add_if_needed(10, "command", 20, 100);
        m.add_if_needed(10, "command", 30, 100);
        m.add_if_needed(10, "command", 25, 100);
        m.add_if_needed(10, "command", 12, 101);
        assert_eq!(m.history(b"command"), vec![(100, 30), (101, 12)]);
        assert_eq!(m.latest(), vec![("command", 101, 12, 30)]);
        assert_eq!(m.reset(Some(b"COMMAND")), 1);
        assert_eq!(m.reset(None), 0);
    }

    #[test]
    fn graph_renders_a_sparkline() {
        let mut m = Monitor::default();
        m.add("command", 10, 100);
        m.add("command", 40, 101);
        let g = m.graph(b"command", 102).unwrap();
        let rows: Vec<&str> = g.split('\n').collect();
        assert_eq!(rows[0], "command - high 40 ms, low 10 ms (all time high 40 ms)");
        assert_eq!(rows[1], "-".repeat(80));
        // As sparkline.c renders it (4 rows of "_o#" fill, then a blank row and the labels).
        assert_eq!(&rows[2..], &[" #", " |", " |", "_|", "  ", "21", "ss", ""]);
    }
}
