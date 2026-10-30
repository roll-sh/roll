//! Index access patterns (BENCH.md "Index access"): job-queue fetches, keyset pagination,
//! `ORDER BY ... DESC LIMIT`, nested-loop lookups, exclusion-constraint inserts, GiST box,
//! pg_trgm and BRIN searches. `access-load` makes the tables (`--rows`, `--jobs`), `access`
//! runs each case `--n` times (`--only a,b`), reporting latency, the server's CPU per
//! operation and its peak memory.

use super::{Ctx, MemProbe, MemSampler, mb, must, run_script, timed};

const LOAD: &str = "
DROP TABLE IF EXISTS que_jobs, oban_jobs, gw_jobs, sq_ready, items, lookup, shapes,
  words_gin, words_gist, ts_events, bookings, bookings2, docs, skip_t, pts;
CREATE EXTENSION IF NOT EXISTS btree_gist;
CREATE EXTENSION IF NOT EXISTS pg_trgm;
CREATE TABLE que_jobs (priority smallint NOT NULL DEFAULT 100, run_at timestamptz NOT NULL DEFAULT now(),
  id bigserial PRIMARY KEY, job_class text NOT NULL, error_count int NOT NULL DEFAULT 0, last_error text,
  queue text NOT NULL DEFAULT 'default', args jsonb NOT NULL DEFAULT '[]', finished_at timestamptz, expired_at timestamptz);
CREATE INDEX que_poll_idx ON que_jobs (queue, priority, run_at, id) WHERE finished_at IS NULL AND expired_at IS NULL;
INSERT INTO que_jobs (priority, run_at, job_class, args)
  SELECT g % 10, now() - g * interval '1 s', 'Job', jsonb_build_array(g) FROM generate_series(1, $JOBS) g;
CREATE TABLE oban_jobs (id bigserial PRIMARY KEY, state text NOT NULL DEFAULT 'available', queue text NOT NULL DEFAULT 'default',
  worker text NOT NULL, args jsonb NOT NULL DEFAULT '{}', attempt int NOT NULL DEFAULT 0, priority int NOT NULL DEFAULT 0,
  scheduled_at timestamp NOT NULL DEFAULT now(), attempted_at timestamp);
CREATE INDEX oban_jobs_fetch ON oban_jobs (state, queue, priority, scheduled_at, id);
INSERT INTO oban_jobs (worker, priority, scheduled_at) SELECT 'W', g % 4, now() - g * interval '1 s' FROM generate_series(1, $JOBS) g;
CREATE TABLE gw_jobs (id bigserial PRIMARY KEY, task text NOT NULL, priority int NOT NULL DEFAULT 0, run_at timestamptz NOT NULL DEFAULT now(),
  attempts int NOT NULL DEFAULT 0, max_attempts int NOT NULL DEFAULT 25, locked_at timestamptz, locked_by text);
CREATE INDEX gw_jobs_main ON gw_jobs (priority, run_at, id) WHERE locked_at IS NULL;
INSERT INTO gw_jobs (task, priority, run_at) SELECT 't', g % 3, now() - g * interval '1 s' FROM generate_series(1, $JOBS) g;
CREATE TABLE sq_ready (id bigserial PRIMARY KEY, job_id bigint NOT NULL, queue_name text NOT NULL, priority int NOT NULL DEFAULT 0,
  created_at timestamp NOT NULL DEFAULT now());
CREATE INDEX sq_poll_all ON sq_ready (priority, job_id);
CREATE INDEX sq_poll_queue ON sq_ready (queue_name, priority, job_id);
INSERT INTO sq_ready (job_id, queue_name, priority) SELECT g, 'default', g % 5 FROM generate_series(1, $JOBS) g;
CREATE TABLE items (id int PRIMARY KEY, created_at timestamptz NOT NULL, cat int NOT NULL, v text);
INSERT INTO items SELECT g, timestamptz '2026-01-01' + g * interval '1 s', g % 1000, md5(g::text) FROM generate_series(1, $ROWS) g;
CREATE INDEX items_cat ON items (cat, id);
CREATE TABLE lookup (item_id int, cat int);
INSERT INTO lookup SELECT (g * 7919) % $ROWS + 1, g % 1000 FROM generate_series(1, 100) g;
CREATE TABLE shapes (id int, b box);
INSERT INTO shapes SELECT g, box(point((g * 37) % 1000, (g * 91) % 1000), point((g * 37) % 1000 + 3, (g * 91) % 1000 + 3))
  FROM generate_series(1, $SMALL) g;
CREATE INDEX shapes_b ON shapes USING gist (b);
CREATE TABLE words_gin (id int, w text);
INSERT INTO words_gin SELECT g, md5(g::text) FROM generate_series(1, $SMALL) g;
CREATE INDEX words_gin_w ON words_gin USING gin (w gin_trgm_ops);
CREATE TABLE words_gist (id int, w text);
INSERT INTO words_gist SELECT * FROM words_gin;
CREATE INDEX words_gist_w ON words_gist USING gist (w gist_trgm_ops);
CREATE TABLE ts_events (ts timestamptz NOT NULL, v int);
INSERT INTO ts_events SELECT timestamptz '2026-01-01' + g * interval '1 s', g % 100 FROM generate_series(1, $ROWS) g;
CREATE INDEX ts_events_brin ON ts_events USING brin (ts);
CREATE TABLE bookings (id serial, room int, during tsrange, EXCLUDE USING gist (room WITH =, during WITH &&));
CREATE TABLE bookings2 (id serial, during tsrange, EXCLUDE USING gist (during WITH &&));
CREATE TABLE docs (id int PRIMARY KEY, tags int[], body jsonb, meta jsonb, tsv tsvector);
INSERT INTO docs SELECT g, ARRAY[g % 100, g % 1000, 1000 + g % 7],
  jsonb_build_object('user', g % 5000, 'kind', 'k' || (g % 20), 'flags', jsonb_build_array('f' || (g % 50)), 'p' || (g % 500), true),
  jsonb_build_object('a', jsonb_build_object('b', g % 2000)), to_tsvector('simple', 'w' || (g % 3000) || ' x' || (g % 40) || ' common')
  FROM generate_series(1, $SMALL) g;
CREATE INDEX docs_tags ON docs USING gin (tags);
CREATE INDEX docs_body ON docs USING gin (body);
CREATE INDEX docs_meta ON docs USING gin (meta jsonb_path_ops);
CREATE INDEX docs_tsv ON docs USING gin (tsv);
CREATE TABLE skip_t (region int NOT NULL, ts int NOT NULL, v int);
INSERT INTO skip_t SELECT g % 10, g / 10, g FROM generate_series(1, $ROWS) g;
CREATE INDEX skip_t_rt ON skip_t (region, ts);
CREATE TABLE pts (id int, p point);
INSERT INTO pts SELECT g, point((g * 37) % 1000 + (g % 7) / 10.0, (g * 91) % 1000) FROM generate_series(1, $SMALL) g;
CREATE INDEX pts_p ON pts USING gist (p);
VACUUM ANALYZE;
";

pub fn load(cx: &Ctx) {
    let rows: u64 = cx.opt("rows", 1_000_000);
    let jobs: u64 = cx.opt("jobs", 10_000);
    let small: u64 = cx.opt("small", 100_000);
    let sql = LOAD
        .replace("$ROWS", &rows.to_string())
        .replace("$JOBS", &jobs.to_string())
        .replace("$SMALL", &small.to_string());
    let mut c = cx.conn();
    let (_, s) = timed(|| run_script(&mut *c, &sql));
    cx.report("access-load", "all", &[("rows", rows.to_string()), ("jobs", jobs.to_string()), ("secs", format!("{s:.2}"))]);
}

/// The server's CPU time so far, in seconds.
fn cpu_secs(p: &MemProbe) -> f64 {
    match p {
        MemProbe::None => 0.0,
        MemProbe::Pid(pid) => std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .and_then(|s| {
                let f: Vec<&str> = s.rsplit_once(')')?.1.split_whitespace().collect();
                Some((f.get(11)?.parse::<f64>().ok()? + f.get(12)?.parse::<f64>().ok()?) / 100.0)
            })
            .unwrap_or(0.0),
        MemProbe::Cgroup(dir) => std::fs::read_to_string(format!("{dir}/cpu.stat"))
            .ok()
            .and_then(|s| s.lines().find_map(|l| l.strip_prefix("usage_usec ")?.parse::<f64>().ok()))
            .map_or(0.0, |u| u / 1e6),
    }
}

/// (case, SQL run each time; `$K` is replaced by a pseudo-random key in [1, rows]).
const CASES: &[(&str, &str)] = &[
    ("que_fetch", "SELECT * FROM que_jobs WHERE queue = 'default' AND finished_at IS NULL AND expired_at IS NULL \
      AND run_at <= now() ORDER BY priority, run_at, id LIMIT 1 FOR UPDATE SKIP LOCKED"),
    ("oban_fetch", "BEGIN; UPDATE oban_jobs SET state = 'executing', attempted_at = now(), attempt = attempt + 1 \
      WHERE id IN (SELECT id FROM oban_jobs WHERE state = 'available' AND queue = 'default' \
      ORDER BY priority, scheduled_at, id LIMIT 10 FOR UPDATE SKIP LOCKED) RETURNING id; ROLLBACK"),
    ("graphile_fetch", "BEGIN; WITH j AS (SELECT id FROM gw_jobs WHERE locked_at IS NULL AND run_at <= now() \
      AND attempts < max_attempts ORDER BY priority, run_at, id LIMIT 1 FOR UPDATE SKIP LOCKED) \
      UPDATE gw_jobs SET attempts = gw_jobs.attempts + 1, locked_at = now(), locked_by = 'w' FROM j \
      WHERE gw_jobs.id = j.id RETURNING gw_jobs.id; ROLLBACK"),
    ("solid_queue_fetch", "SELECT job_id FROM sq_ready WHERE queue_name = 'default' ORDER BY priority, job_id \
      LIMIT 5 FOR UPDATE SKIP LOCKED"),
    ("keyset_page", "SELECT id, v FROM items WHERE id > $K ORDER BY id LIMIT 20"),
    ("keyset_page_desc", "SELECT id, v FROM items WHERE id < $K ORDER BY id DESC LIMIT 20"),
    ("order_desc_limit", "SELECT id, v FROM items ORDER BY id DESC LIMIT 10"),
    ("max_id", "SELECT max(id) FROM items"),
    ("cat_limit", "SELECT id FROM items WHERE cat = $K % 1000 ORDER BY id LIMIT 5"),
    ("any_lookup", "SELECT count(*) FROM items WHERE id = ANY(ARRAY[$K, $K + 7, $K + 1000, $K + 5000, $K + 9999])"),
    ("nested_loop", "SELECT count(*), max(i.v) FROM lookup l JOIN items i ON i.id = l.item_id"),
    ("exists_limit", "SELECT EXISTS (SELECT 1 FROM items WHERE cat = $K % 1000 AND id > 10)"),
    ("gist_box", "SELECT count(*) FROM shapes WHERE b && box(point($K % 990, $K % 977), point($K % 990 + 10, $K % 977 + 10))"),
    ("trgm_gin_like", "SELECT count(*) FROM words_gin WHERE w LIKE '%' || substr(md5($K::text), 3, 5) || '%'"),
    ("trgm_gist_like", "SELECT count(*) FROM words_gist WHERE w LIKE '%' || substr(md5($K::text), 3, 5) || '%'"),
    ("trgm_gin_sim", "SELECT count(*) FROM words_gin WHERE w % md5($K::text)"),
    ("trgm_gin_word", "SELECT count(*) FROM words_gin WHERE w %> substr(md5($K::text), 1, 8)"),
    ("gin_array", "SELECT count(*) FROM docs WHERE tags @> ARRAY[$K % 1000]"),
    ("gin_array_overlap", "SELECT count(*) FROM docs WHERE tags && ARRAY[$K % 1000, ($K + 1) % 1000]"),
    ("gin_jsonb", "SELECT count(*) FROM docs WHERE body @> jsonb_build_object('user', $K % 5000)"),
    ("gin_jsonb_exists", "SELECT count(*) FROM docs WHERE body ? ('p' || ($K % 500))"),
    ("gin_jsonb_path", "SELECT count(*) FROM docs WHERE meta @> jsonb_build_object('a', jsonb_build_object('b', $K % 2000))"),
    ("gin_tsvector", "SELECT count(*) FROM docs WHERE tsv @@ to_tsquery('simple', 'w' || ($K % 3000))"),
    ("gin_tsvector_and", "SELECT count(*) FROM docs WHERE tsv @@ to_tsquery('simple', 'x' || ($K % 40) || ' & common')"),
    ("skip_scan", "SELECT count(*) FROM skip_t WHERE ts = $K % 100000"),
    ("gist_point_box", "SELECT id FROM pts WHERE p <@ box(point($K % 990, $K % 977), point($K % 990 + 10, $K % 977 + 10)) LIMIT 5"),
    ("gist_point_knn", "SELECT id FROM pts ORDER BY p <-> point($K % 1000, $K % 977) LIMIT 5"),
    ("brin_range", "SELECT count(*), sum(v) FROM ts_events WHERE ts >= timestamptz '2026-01-01' + $K * interval '1 s' \
      AND ts < timestamptz '2026-01-01' + ($K + 3600) * interval '1 s'"),
    ("booking_insert", "INSERT INTO bookings (room, during) VALUES ($K % 50, tsrange(timestamp '2026-01-01' + $K * interval '1 hour', \
      timestamp '2026-01-01' + ($K + 1) * interval '1 hour'))"),
    ("booking2_insert", "INSERT INTO bookings2 (during) VALUES (tsrange(timestamp '2026-01-01' + $K * interval '1 hour', \
      timestamp '2026-01-01' + ($K + 1) * interval '1 hour'))"),
    // 3,000 bookings in one statement, and one at a time in one transaction.
    ("booking_bulk", "TRUNCATE bookings2; INSERT INTO bookings2 (during) SELECT tsrange(timestamp '2026-01-01' + g * interval '1 hour', \
      timestamp '2026-01-01' + (g + 1) * interval '1 hour') FROM generate_series(1, 3000) g"),
    ("booking_loop", "TRUNCATE bookings; DO $$ BEGIN FOR i IN 1..3000 LOOP INSERT INTO bookings (room, during) VALUES (i % 50, \
      tsrange(timestamp '2026-01-01' + i * interval '1 hour', timestamp '2026-01-01' + (i + 1) * interval '1 hour')); END LOOP; END $$"),
];

pub fn run(cx: &Ctx) {
    let n: usize = cx.opt("n", 200);
    let rows: u64 = cx.opt("rows", 1_000_000);
    let only: String = cx.opt("only", String::new());
    let mut c = cx.conn();
    for (name, sql) in CASES {
        if !only.is_empty() && !only.split(',').any(|o| o == *name) {
            continue;
        }
        let insert = name.ends_with("_insert");
        if insert {
            must(c.simple("TRUNCATE bookings, bookings2"), "truncate");
        }
        // Keys: a fixed pseudo-random walk (inserts: 0, 1, 2, ... so bookings never overlap).
        let key = |i: usize| if insert { i as u64 } else { (i as u64 * 2_654_435_761) % rows + 1 };
        let warm = if insert { 0 } else { 5.min(n) };
        for i in 0..warm {
            must(c.simple(&sql.replace("$K", &key(i).to_string())), name);
        }
        let mut lat = Vec::with_capacity(n);
        let sampler = MemSampler::start(&cx.mem);
        let cpu0 = cpu_secs(&cx.mem);
        let (_, total) = timed(|| {
            for i in 0..n {
                let q = sql.replace("$K", &key(i + warm).to_string());
                let (r, s) = timed(|| c.simple(&q));
                must(r, name);
                lat.push(s);
            }
        });
        let cpu = cpu_secs(&cx.mem) - cpu0;
        let peak = sampler.stop();
        lat.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let ms = |p: f64| format!("{:.3}", lat[((lat.len() - 1) as f64 * p) as usize] * 1e3);
        cx.report(
            "access",
            name,
            &[
                ("n", n.to_string()),
                ("median_ms", ms(0.5)),
                ("p95_ms", ms(0.95)),
                ("total_s", format!("{total:.3}")),
                ("cpu_ms_per_op", format!("{:.3}", cpu * 1e3 / n as f64)),
                ("peak_mem_mb", mb(peak)),
            ],
        );
    }
}

