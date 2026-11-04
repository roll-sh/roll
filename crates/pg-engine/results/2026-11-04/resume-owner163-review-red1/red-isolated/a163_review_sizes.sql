-- Serial builds must form INCLUDE values too, before sorting/checking uniqueness.
SET max_parallel_maintenance_workers = 0;
CREATE FUNCTION a163_review_text(n int) RETURNS text LANGUAGE sql IMMUTABLE
AS $$ SELECT left(string_agg(md5(g::text), ''), n) FROM generate_series(1, (n + 31) / 32) g $$;
CREATE TABLE a163_review_include (k int, payload text);
INSERT INTO a163_review_include VALUES (1, a163_review_text(2800));
CREATE INDEX a163_review_i ON a163_review_include (k) INCLUDE (payload);
DROP TABLE a163_review_include;
CREATE TABLE a163_review_include (k int, payload text);
INSERT INTO a163_review_include VALUES (1, a163_review_text(8300));
CREATE INDEX a163_review_i ON a163_review_include (k) INCLUDE (payload);
-- A duplicate precedes an unformable heap row: formation wins over uniqueness.
DROP TABLE a163_review_include;
CREATE TABLE a163_review_include (k int, payload text);
INSERT INTO a163_review_include VALUES (1, 'first'), (1, 'second'), (2, a163_review_text(8300));
CREATE UNIQUE INDEX a163_review_i ON a163_review_include (k) INCLUDE (payload);
DELETE FROM a163_review_include WHERE k = 2;
CREATE UNIQUE INDEX a163_review_i ON a163_review_include (k) INCLUDE (payload);
DROP TABLE a163_review_include;
-- bpchar storage omits trailing blanks internally; sizing must include declared padding.
CREATE TABLE a163_review_char (c char(2800));
INSERT INTO a163_review_char VALUES (a163_review_text(2650));
CREATE INDEX a163_review_c ON a163_review_char (c);
TRUNCATE a163_review_char;
CREATE INDEX a163_review_c ON a163_review_char (c);
INSERT INTO a163_review_char VALUES (a163_review_text(2650));
DROP TABLE a163_review_char;
-- Probe GIN build error order: the first heap row has the larger sorted key.
CREATE TABLE a163_review_gin (a text[]);
INSERT INTO a163_review_gin VALUES (ARRAY['z' || a163_review_text(2900)]), (ARRAY['a' || a163_review_text(2800)]);
CREATE INDEX a163_review_g ON a163_review_gin USING gin (a);
DROP TABLE a163_review_gin;
DROP FUNCTION a163_review_text;
RESET max_parallel_maintenance_workers;
