-- createdb: LOCALE 'C'
-- The libc C locale (initdb --locale=C, the default of a new cluster): case mapping,
-- ILIKE and regular expressions know ASCII letters only, strings sort by bytes.
SELECT datlocprovider, datcollate, datctype, datlocale, datcollversion FROM pg_database WHERE datname = current_database();
SELECT upper('é ß ǆ ﬁ i'), lower('É İ Σ ǅ I'), initcap('éa bé ǆx hello-WORLD 1st ßa'), casefold('ß İ Σ ﬁ AB');
SELECT 'é' ~ '[[:alpha:]]', 'É' ~* 'é', 'É' ILIKE 'é', 'E' ILIKE 'e', '٣' ~ '[[:digit:]]', 'ǅ' ~ '[[:upper:]]', 'ß' ~ '^\w$';
SELECT regexp_replace('ÀÉÎõü', '[[:lower:]]', 'x', 'g'), regexp_replace('ÀÉÎõüab', '[a-z]', 'x', 'gi'), 'aÉ' SIMILAR TO 'A%' , substring('xÉy' from '[[:alpha:]]+');
SELECT string_agg(x, ' ' ORDER BY x) FROM (VALUES ('a'),('B'),('é'),('E'),('e'),('Z'),('_'),('1'),('ä'),('z'),('Ä')) v(x);
SELECT pg_collation_actual_version(100), collprovider FROM pg_collation WHERE oid = 100;
SELECT jsonb_path_query_array('["é","É","e","E"]', '$[*] ? (@ like_regex "^[[:upper:]]")'), jsonb_path_query_array('["é","É","e"]', '$[*] ? (@ like_regex "^é" flag "i")');
SELECT to_tsvector('simple', 'Éclair naïve café ÉCOLE École');
SELECT upper(x) COLLATE "pg_c_utf8", upper(x COLLATE "pg_unicode_fast"), upper(x COLLATE "und-x-icu"), lower(x COLLATE "C") FROM (VALUES ('ßé')) v(x);
SELECT 'é' ~ '[[:alpha:]]' COLLATE "pg_c_utf8", 'É' ILIKE 'é' COLLATE "und-x-icu", 'É' ~* 'é' COLLATE "pg_unicode_fast";
CREATE TABLE a60_lc (t text);
INSERT INTO a60_lc VALUES ('b'), ('É'), ('a'), ('é'), ('B'), ('ab');
CREATE INDEX ON a60_lc (t);
SET enable_seqscan = off;
EXPLAIN (COSTS OFF) SELECT * FROM a60_lc WHERE t LIKE 'a%';
SELECT * FROM a60_lc WHERE t LIKE 'a%' ORDER BY t;
SELECT * FROM a60_lc WHERE t ILIKE 'é' ORDER BY t;
RESET enable_seqscan;
SELECT name, setting FROM pg_settings WHERE name IN ('lc_messages', 'lc_monetary', 'lc_numeric', 'lc_time') ORDER BY 1;
DROP TABLE a60_lc;
