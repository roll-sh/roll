-- Foreign keys: type compatibility, ON DELETE SET NULL / DEFAULT (columns), MATCH FULL
-- validation, system columns, deferred checks of rows updated in the same transaction,
-- SET DEFAULT rechecks, pending trigger events, partitions attaching keys.
CREATE TABLE gd_fk_pk (p1 numeric PRIMARY KEY, p2 varchar(4) UNIQUE, p3 real UNIQUE);
INSERT INTO gd_fk_pk VALUES (42, 'a', 1.5);
CREATE TABLE gd_fk_fk (f1 int, f2 varchar(4), f3 real);
INSERT INTO gd_fk_fk VALUES (42, 'a', 1.5);
ALTER TABLE gd_fk_fk ADD FOREIGN KEY (f1) REFERENCES gd_fk_pk;
INSERT INTO gd_fk_fk VALUES (43);
ALTER TABLE gd_fk_fk ADD CONSTRAINT gd_fk_bad FOREIGN KEY (f2) REFERENCES gd_fk_pk (p3);
ALTER TABLE gd_fk_fk ADD CONSTRAINT gd_fk_bad FOREIGN KEY (f1) REFERENCES gd_fk_pk (p2);
CREATE TABLE gd_fk_sys (f1 int, FOREIGN KEY (tableoid) REFERENCES gd_fk_pk (p1));
DROP TABLE gd_fk_fk, gd_fk_pk;
-- SET NULL / SET DEFAULT on a subset of the key
CREATE TABLE gd_fk_p (tid int, id int, PRIMARY KEY (tid, id));
CREATE TABLE gd_fk_c (tid int, id int, foo int, FOREIGN KEY (tid, id) REFERENCES gd_fk_p ON DELETE SET NULL (bar));
CREATE TABLE gd_fk_c (tid int, id int, foo int, FOREIGN KEY (tid, id) REFERENCES gd_fk_p ON DELETE SET NULL (foo));
CREATE TABLE gd_fk_c (
  tid int, id int, n int, d int DEFAULT 0,
  FOREIGN KEY (tid, n) REFERENCES gd_fk_p ON DELETE SET NULL (n),
  FOREIGN KEY (tid, d) REFERENCES gd_fk_p ON DELETE SET DEFAULT (d, d));
SELECT pg_get_constraintdef(oid), confdelsetcols FROM pg_constraint
 WHERE conrelid = 'gd_fk_c'::regclass ORDER BY oid;
INSERT INTO gd_fk_p VALUES (1, 0), (1, 1), (1, 2);
INSERT INTO gd_fk_c VALUES (1, 1, 1, NULL), (1, 2, NULL, 2);
DELETE FROM gd_fk_p WHERE id = 1 OR id = 2;
SELECT * FROM gd_fk_c ORDER BY id;
DROP TABLE gd_fk_c, gd_fk_p;
-- MATCH FULL over existing rows
CREATE TABLE gd_fk_mp (a int, b int, PRIMARY KEY (a, b));
CREATE TABLE gd_fk_mf (a int, b int);
INSERT INTO gd_fk_mf VALUES (1, NULL);
ALTER TABLE gd_fk_mf ADD FOREIGN KEY (a, b) REFERENCES gd_fk_mp MATCH FULL;
DROP TABLE gd_fk_mf, gd_fk_mp;
-- a deferred check isn't lost when the row is updated in the same transaction
CREATE TABLE gd_fk_dp (id int PRIMARY KEY);
CREATE TABLE gd_fk_df (id int PRIMARY KEY, fk int REFERENCES gd_fk_dp DEFERRABLE INITIALLY DEFERRED);
BEGIN;
INSERT INTO gd_fk_df VALUES (0, 20);
UPDATE gd_fk_df SET id = id + 1;
COMMIT;
BEGIN;
INSERT INTO gd_fk_df VALUES (2, 20);
ALTER TABLE gd_fk_df DROP CONSTRAINT gd_fk_df_fk_fkey;
ROLLBACK;
DROP TABLE gd_fk_df, gd_fk_dp;
-- SET DEFAULT back to the key that went
CREATE TABLE gd_fk_sp (f1 int PRIMARY KEY);
CREATE TABLE gd_fk_sc (f1 int DEFAULT 0 REFERENCES gd_fk_sp ON DELETE SET DEFAULT);
INSERT INTO gd_fk_sp VALUES (0), (1), (2);
INSERT INTO gd_fk_sc VALUES (2);
DELETE FROM gd_fk_sp WHERE f1 = 2;
DELETE FROM gd_fk_sp WHERE f1 = 0;
SELECT * FROM gd_fk_sc;
DROP TABLE gd_fk_sc, gd_fk_sp;
-- keys that look different but compare equal cascade
CREATE TABLE gd_fk_zp (a float8 PRIMARY KEY);
CREATE TABLE gd_fk_zf (x float8 REFERENCES gd_fk_zp ON UPDATE CASCADE);
INSERT INTO gd_fk_zp VALUES ('-0');
INSERT INTO gd_fk_zf VALUES ('-0');
UPDATE gd_fk_zp SET a = '0' WHERE a = '-0';
SELECT * FROM gd_fk_zf;
DROP TABLE gd_fk_zf, gd_fk_zp;
-- partitioned tables
CREATE TABLE gd_fk_np (a int, b int, PRIMARY KEY (a, b));
CREATE TABLE gd_fk_pf (a int, b int) PARTITION BY RANGE (a, b);
ALTER TABLE ONLY gd_fk_pf ADD FOREIGN KEY (a, b) REFERENCES gd_fk_np;
ALTER TABLE gd_fk_pf ADD FOREIGN KEY (a, b) REFERENCES gd_fk_np NOT VALID;
CREATE TABLE gd_fk_pf1 (a int, b int);
INSERT INTO gd_fk_pf1 VALUES (1000, 1000);
ALTER TABLE gd_fk_pf1 ADD FOREIGN KEY (a, b) REFERENCES gd_fk_np NOT VALID;
ALTER TABLE gd_fk_pf ATTACH PARTITION gd_fk_pf1 FOR VALUES FROM (1000, 1000) TO (2000, 2000);
ALTER TABLE gd_fk_pf VALIDATE CONSTRAINT gd_fk_pf_a_b_fkey;
ALTER TABLE gd_fk_pf ATTACH PARTITION gd_fk_pf1 FOR VALUES FROM (1000, 1000) TO (2000, 2000);
INSERT INTO gd_fk_np VALUES (1000, 1000);
ALTER TABLE gd_fk_pf ATTACH PARTITION gd_fk_pf1 FOR VALUES FROM (1000, 1000) TO (2000, 2000);
SELECT conrelid::regclass, conname, convalidated, conparentid <> 0 AS child FROM pg_constraint
 WHERE conrelid IN ('gd_fk_pf'::regclass, 'gd_fk_pf1'::regclass) AND contype = 'f' ORDER BY 1, 2;
CREATE TABLE gd_fk_pf2 (a int, b int, CONSTRAINT gd_fk_c2 FOREIGN KEY (a, b) REFERENCES gd_fk_np NOT ENFORCED);
ALTER TABLE gd_fk_pf ATTACH PARTITION gd_fk_pf2 FOR VALUES FROM (2000, 2000) TO (3000, 3000);
DROP TABLE gd_fk_pf, gd_fk_pf2, gd_fk_np;
CREATE TABLE gd_fk_rp (a int PRIMARY KEY);
CREATE TABLE gd_fk_rf (a int REFERENCES gd_fk_rp) PARTITION BY LIST (a);
ALTER TABLE gd_fk_rf ATTACH PARTITION gd_fk_rp FOR VALUES IN (1);
DROP TABLE gd_fk_rf, gd_fk_rp;
-- a self-referencing partitioned key: one constraint per partition, named by level
CREATE TABLE gd_fk_self (id int PRIMARY KEY, ref int REFERENCES gd_fk_self) PARTITION BY RANGE (id);
CREATE TABLE gd_fk_self1 PARTITION OF gd_fk_self FOR VALUES FROM (0) TO (10);
CREATE TABLE gd_fk_self3 PARTITION OF gd_fk_self FOR VALUES FROM (20) TO (40) PARTITION BY RANGE (id);
CREATE TABLE gd_fk_self31 PARTITION OF gd_fk_self3 FOR VALUES FROM (20) TO (30);
SELECT co.conname, cf.relname AS ref FROM pg_constraint co JOIN pg_class cf ON cf.oid = co.confrelid
 WHERE co.conrelid = 'gd_fk_self'::regclass AND co.contype = 'f' ORDER BY 1;
DROP TABLE gd_fk_self;
-- a row can't move out from under a non-root ancestor a key points at
CREATE TABLE gd_fk_xp (a int PRIMARY KEY) PARTITION BY LIST (a);
CREATE TABLE gd_fk_xp1 PARTITION OF gd_fk_xp FOR VALUES IN (1, 2) PARTITION BY LIST (a);
CREATE TABLE gd_fk_xp11 PARTITION OF gd_fk_xp1 FOR VALUES IN (1);
CREATE TABLE gd_fk_xp12 PARTITION OF gd_fk_xp1 FOR VALUES IN (2);
CREATE TABLE gd_fk_xp2 PARTITION OF gd_fk_xp FOR VALUES IN (3);
CREATE TABLE gd_fk_xf (a int REFERENCES gd_fk_xp1 ON UPDATE CASCADE);
INSERT INTO gd_fk_xp VALUES (2), (3);
UPDATE gd_fk_xp SET a = a - 1;
UPDATE gd_fk_xp1 SET a = a - 1;
SELECT tableoid::regclass, a FROM gd_fk_xp ORDER BY a;
DROP TABLE gd_fk_xf, gd_fk_xp;
