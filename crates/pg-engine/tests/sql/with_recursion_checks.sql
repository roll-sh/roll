CREATE TEMP TABLE gx_dept (id INTEGER PRIMARY KEY, parent_gx_dept INTEGER REFERENCES gx_dept, name TEXT);
INSERT INTO gx_dept VALUES (0, NULL, 'ROOT'), (1, 0, 'A'), (2, 1, 'B'), (3, 2, 'C'), (4, 2, 'D'), (5, 0, 'E'), (6, 4, 'F'), (7, 5, 'G');
with recursive q as (
      select * from gx_dept
    union all
      (with recursive x as (
           select * from gx_dept
         union all
           (select * from q union all select * from x)
        )
       select * from x)
    )
select * from q limit 24;
CREATE TEMP TABLE bug6051 AS select i from generate_series(1,3) as t(i);
WITH t1 AS ( DELETE FROM bug6051 RETURNING * ) INSERT INTO bug6051 SELECT * FROM t1;
SELECT * FROM bug6051;
CREATE TEMP TABLE m (k int, v text);
WITH RECURSIVE cte_basic AS (SELECT 1 a, 'cte_basic val' b)
MERGE INTO m USING (select 0 k, 'merge source SubPlan' v) o ON m.k=o.k
WHEN MATCHED THEN UPDATE SET v = (SELECT b || ' merge update' FROM cte_basic WHERE cte_basic.a = m.k LIMIT 1)
WHEN NOT MATCHED THEN INSERT VALUES(o.k, o.v);
WITH RECURSIVE x(id) AS (values (1) UNION ALL SELECT (SELECT * FROM x) FROM x WHERE id < 5) SELECT * FROM x;
WITH RECURSIVE
  x (id) AS (SELECT 1 UNION ALL SELECT id+1 FROM y WHERE id < 5),
  y (id) AS (SELECT 1 UNION ALL SELECT id+1 FROM x WHERE id < 5)
SELECT * FROM x;
WITH RECURSIVE foo(i) AS (values (1) UNION ALL (SELECT i+1 FROM foo WHERE i < 10 EXCEPT SELECT i+1 FROM foo WHERE i < 5)) SELECT * FROM foo;
WITH RECURSIVE foo(i) AS (values (1) UNION ALL SELECT * FROM (SELECT i+1 FROM foo WHERE i < 10 UNION ALL SELECT i+1 FROM foo WHERE i < 5) AS t) SELECT * FROM foo;
WITH RECURSIVE x(n) AS (SELECT 1 UNION ALL SELECT count(*) FROM x) SELECT * FROM x;
WITH RECURSIVE x(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM x LIMIT 10 OFFSET 1) SELECT * FROM x;
WITH RECURSIVE x(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM x FOR UPDATE) SELECT * FROM x;
WITH RECURSIVE x(id) AS (SELECT 1 UNION ALL SELECT x.id+1 FROM generate_series(1,3) g LEFT JOIN x ON true WHERE x.id < 3) SELECT * FROM x;
WITH RECURSIVE a AS (SELECT * FROM b), b AS (SELECT 1 AS v) SELECT * FROM a;
WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM t) SELECT * FROM t LIMIT 5;
CREATE TABLE gx_t1 (a bigint);
CREATE TABLE gx_t1a (a bigint);
CREATE VIEW gx_v2(a,b) AS SELECT * FROM gx_t1, gx_t1a;
CREATE VIEW gx_v3 AS SELECT * from gx_v2;
CREATE OR REPLACE VIEW gx_v2 AS SELECT * from gx_v3;
BEGIN;
LOCK TABLE gx_v2 IN EXCLUSIVE MODE;
ROLLBACK;
CREATE VIEW gx_v7 AS SELECT * from gx_v2;
SELECT * FROM gx_v7;
SELECT * FROM gx_v3;
DROP VIEW gx_v7;
DROP VIEW gx_v3;
DROP VIEW gx_v3 CASCADE;
DROP VIEW gx_v2;
DROP TABLE gx_t1, gx_t1a;
SET client_min_messages = warning;
DROP TABLE IF EXISTS gx_nothere;
RESET client_min_messages;
