-- setup for the apply-like workload (152's shape) and 144's skip scan
DROP TABLE IF EXISTS acc, hist, skip_t;
CREATE TABLE acc (aid int PRIMARY KEY, abalance int, filler char(84));
INSERT INTO acc SELECT g, 0, '' FROM generate_series(1, 10000) g;
CREATE TABLE hist (hid bigserial PRIMARY KEY, aid int, delta int, mtime timestamp);
CREATE INDEX hist_aid ON hist (aid);
CREATE TABLE skip_t (g int, ts int, v int);
INSERT INTO skip_t SELECT g % 100, g / 100, g FROM generate_series(1, 1000000) g;
CREATE INDEX skip_t_g_ts ON skip_t (g, ts);
VACUUM ANALYZE acc, hist, skip_t;
