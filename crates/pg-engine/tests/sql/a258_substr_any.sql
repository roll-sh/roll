-- substring(col FROM start FOR count) = ANY (array) / <> ALL as a batch filter, of a text
-- column and of a bpchar column cast to text (trailing spaces gone first), multibyte characters,
-- starts before 1, a zero count, a NULL item, against the rows PostgreSQL gives.
CREATE TABLE a258_k (id int, f char(1), s char(1), c char(10), v varchar(20), d date, b bool, q numeric(15,2), p numeric(15,2), t numeric(15,2), big numeric(30,2), k int) WITH (autovacuum_enabled = false);
INSERT INTO a258_k SELECT s,
  CASE WHEN s % 97 = 0 THEN NULL ELSE substr('ANR', 1 + s % 3, 1) END,
  CASE WHEN s % 89 = 0 THEN NULL ELSE substr('FO', 1 + s % 2, 1) END,
  CASE WHEN s % 83 = 0 THEN NULL ELSE (ARRAY['13-55', 'é1-x', '31', 'ab cd ef', 'zz', ' 1', '29-éé-00', '17'])[1 + s % 8] END,
  CASE WHEN s % 79 = 0 THEN NULL ELSE (ARRAY['ü', 'üü', 'abcdef', 'abcdefg', '', 'x y'])[1 + s % 6] END,
  CASE WHEN s % 73 = 0 THEN NULL ELSE date '1992-01-01' + (s % 2500) END,
  CASE WHEN s % 71 = 0 THEN NULL ELSE s % 2 = 0 END,
  CASE WHEN s % 67 = 0 THEN NULL ELSE ((s % 5000) - 1000) * 0.01 END,
  (s % 104950) + 900.5,
  (s % 11) * 0.01,
  CASE WHEN s % 5 = 0 THEN 123456789012345678.91 + s ELSE s * 1.5 END,
  s % 40
  FROM generate_series(1, 120000) s;
VACUUM ANALYZE a258_k;
SELECT count(*), sum(q) FROM a258_k WHERE substring(c FROM 1 FOR 2) = ANY ('{13,é1,31,ab,17}');
SELECT count(*), sum(q) FROM a258_k WHERE substring(c FROM 3 FOR 2) = ANY ('{55,-x," c",-é}');
SELECT count(*), sum(q) FROM a258_k WHERE substring(c FROM 0 FOR 3) = ANY (ARRAY['13', 'é1', 'ab', ' 1']);
SELECT count(*), sum(q) FROM a258_k WHERE substring(c FROM 2 FOR 0) = ANY (ARRAY['', 'x']);
SELECT count(*), sum(q) FROM a258_k WHERE substring(c FROM 1 FOR 2) <> ALL ('{13,31,zz}');
SELECT count(*), sum(q) FROM a258_k WHERE substring(c FROM 1 FOR 2) = ANY (ARRAY['13', NULL]);
SELECT count(*), sum(p) FROM a258_k WHERE substring(v FROM 2 FOR 2) = ANY ('{ü,bc," y"}') AND q > 1;
SELECT substring(c FROM 1 FOR 2) AS cc, count(*), sum(q) FROM a258_k WHERE substring(c FROM 1 FOR 2) = ANY ('{13,é1,29,17, 1}') GROUP BY 1 ORDER BY 1;
DROP TABLE a258_k;
