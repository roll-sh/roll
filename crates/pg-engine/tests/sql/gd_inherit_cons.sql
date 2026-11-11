-- Inheritance and constraint merging: NOT VALID / NOT ENFORCED rules, not-null names across
-- parents, NO INHERIT drops, typed tables, DETACH CONCURRENTLY, partition-key columns.
CREATE TABLE gd_ic_p1 (f1 int CONSTRAINT gd_ic_ck CHECK (f1 > 0));
CREATE TABLE gd_ic_p2 (f1 int CONSTRAINT gd_ic_ck CHECK (f1 > 0) NOT ENFORCED);
-- parents differing in enforcement merge into an enforced constraint
CREATE TABLE gd_ic_c1 () INHERITS (gd_ic_p1, gd_ic_p2);
SELECT conname, conenforced, convalidated, coninhcount FROM pg_constraint
 WHERE conrelid = 'gd_ic_c1'::regclass AND contype = 'c';
-- but a NOT ENFORCED local one can't take an enforced parent's
CREATE TABLE gd_ic_c2 (f1 int CONSTRAINT gd_ic_ck CHECK (f1 > 0) NOT ENFORCED) INHERITS (gd_ic_p1);
DROP TABLE gd_ic_c1, gd_ic_p1, gd_ic_p2;
-- a valid parent constraint over a child's NOT VALID one
CREATE TABLE gd_ic_v (f1 int);
CREATE TABLE gd_ic_vc () INHERITS (gd_ic_v);
ALTER TABLE gd_ic_vc ADD CONSTRAINT gd_ic_vck CHECK (f1 > 0) NOT VALID;
ALTER TABLE gd_ic_v ADD CONSTRAINT gd_ic_vck CHECK (f1 > 0);
ALTER TABLE gd_ic_v ADD CONSTRAINT gd_ic_vck CHECK (f1 > 0) NOT VALID;
SELECT conrelid::regclass, convalidated, conislocal, coninhcount FROM pg_constraint
 WHERE conname = 'gd_ic_vck' ORDER BY 1;
DROP TABLE gd_ic_v CASCADE;
-- not-null constraints named the same in two parents
CREATE TABLE gd_ic_n1 (a int CONSTRAINT gd_ic_nn NOT NULL);
CREATE TABLE gd_ic_n2 (b int CONSTRAINT gd_ic_nn NOT NULL);
CREATE TABLE gd_ic_n3 (CONSTRAINT gd_ic_foo NOT NULL a) INHERITS (gd_ic_n1, gd_ic_n2);
SELECT conname, conkey, conislocal, coninhcount FROM pg_constraint
 WHERE conrelid = 'gd_ic_n3'::regclass ORDER BY conname;
DROP TABLE gd_ic_n1, gd_ic_n2 CASCADE;
-- dropping a NO INHERIT not-null leaves the children's
CREATE TABLE gd_ic_ni (f1 int NOT NULL NO INHERIT);
CREATE TABLE gd_ic_nic (f1 int NOT NULL) INHERITS (gd_ic_ni);
ALTER TABLE gd_ic_ni ALTER CONSTRAINT gd_ic_ni_f1_not_null INHERIT;
ALTER TABLE gd_ic_ni ALTER CONSTRAINT gd_ic_ni_f1_not_null NO INHERIT;
ALTER TABLE gd_ic_ni DROP CONSTRAINT gd_ic_ni_f1_not_null;
SELECT conrelid::regclass, coninhcount, conislocal FROM pg_constraint
 WHERE contype = 'n' AND conrelid::regclass::text LIKE 'gd_ic_ni%' ORDER BY 1;
DROP TABLE gd_ic_ni CASCADE;
-- a child is checked when the parent's constraint is dropped from it
DROP ROLE IF EXISTS gd_ic_alice, gd_ic_bob;
CREATE ROLE gd_ic_alice;
CREATE ROLE gd_ic_bob;
GRANT CREATE ON SCHEMA public TO gd_ic_alice, gd_ic_bob;
GRANT gd_ic_alice TO gd_ic_bob;
SET SESSION AUTHORIZATION gd_ic_alice;
CREATE TABLE gd_ic_own (a int NOT NULL);
SET SESSION AUTHORIZATION gd_ic_bob;
CREATE TABLE gd_ic_ownc () INHERITS (gd_ic_own);
SET SESSION AUTHORIZATION gd_ic_alice;
ALTER TABLE gd_ic_own ALTER a DROP NOT NULL;
SET SESSION AUTHORIZATION gd_ic_bob;
ALTER TABLE gd_ic_own ALTER a DROP NOT NULL;
RESET SESSION AUTHORIZATION;
DROP TABLE gd_ic_own CASCADE;
REVOKE CREATE ON SCHEMA public FROM gd_ic_alice, gd_ic_bob;
DROP ROLE gd_ic_alice, gd_ic_bob;
-- typed tables: options once per column; ALTER TYPE reaches inheritors; dependent checks
CREATE TYPE gd_ic_ty AS (a int, b text);
CREATE TABLE gd_ic_typed OF gd_ic_ty (b WITH OPTIONS NOT NULL, b WITH OPTIONS DEFAULT '');
CREATE TABLE gd_ic_typed OF gd_ic_ty;
CREATE TABLE gd_ic_typedc () INHERITS (gd_ic_typed);
ALTER TABLE gd_ic_typedc OF gd_ic_ty;
ALTER TYPE gd_ic_ty ADD ATTRIBUTE c text CASCADE;
SELECT attname FROM pg_attribute WHERE attrelid = 'gd_ic_typedc'::regclass AND attnum > 0 ORDER BY attnum;
DROP TABLE gd_ic_typed CASCADE;
CREATE TABLE gd_ic_tx (x int, y gd_ic_ty CHECK ((y).a > 0));
SELECT conname FROM pg_constraint WHERE conrelid = 'gd_ic_tx'::regclass;
ALTER TYPE gd_ic_ty DROP ATTRIBUTE a;
ALTER TYPE gd_ic_ty DROP ATTRIBUTE a CASCADE;
SELECT count(*) FROM pg_constraint WHERE conrelid = 'gd_ic_tx'::regclass;
DROP TABLE gd_ic_tx;
-- a table can't hold its own row type
CREATE TABLE gd_ic_rec (f1 int);
ALTER TABLE gd_ic_rec ADD COLUMN f2 gd_ic_rec;
ALTER TABLE gd_ic_rec ADD COLUMN f2 gd_ic_rec[];
CREATE TABLE gd_ic_rec2 (f1 int, f2 gd_ic_rec);
ALTER TABLE gd_ic_rec ADD COLUMN f2 gd_ic_rec2;
DROP TABLE gd_ic_rec2, gd_ic_rec;
DROP TYPE gd_ic_ty;
-- partitions: ONLY SET NOT NULL, a sub-partition's key, the failing leaf's name
CREATE TABLE gd_ic_lp (a int, b char(1)) PARTITION BY LIST (a);
CREATE TABLE gd_ic_lp5 PARTITION OF gd_ic_lp FOR VALUES IN (5) PARTITION BY LIST (b);
CREATE TABLE gd_ic_lp5a PARTITION OF gd_ic_lp5 FOR VALUES IN ('a');
ALTER TABLE ONLY gd_ic_lp ALTER b SET NOT NULL;
ALTER TABLE gd_ic_lp DROP COLUMN b;
ALTER TABLE gd_ic_lp ALTER COLUMN b TYPE text;
CREATE TABLE gd_ic_lp6 (a int, b char(1)) PARTITION BY LIST (b);
CREATE TABLE gd_ic_lp6a PARTITION OF gd_ic_lp6 FOR VALUES IN ('a');
INSERT INTO gd_ic_lp6 VALUES (7, 'a');
ALTER TABLE gd_ic_lp ATTACH PARTITION gd_ic_lp6 FOR VALUES IN (6);
-- DETACH CONCURRENTLY leaves a constraint in the partition constraint's place
CREATE TABLE gd_ic_rp (a int) PARTITION BY RANGE (a);
CREATE TABLE gd_ic_rp1 PARTITION OF gd_ic_rp FOR VALUES FROM (0) TO (100);
CREATE TABLE gd_ic_rp2 PARTITION OF gd_ic_rp (CHECK (a >= 123 AND a < 133 AND a IS NOT NULL))
  FOR VALUES FROM (100) TO (200);
ALTER TABLE gd_ic_rp DETACH PARTITION gd_ic_rp1 CONCURRENTLY;
ALTER TABLE gd_ic_rp DETACH PARTITION gd_ic_rp2 CONCURRENTLY;
SELECT conrelid::regclass, conname, pg_get_constraintdef(oid) FROM pg_constraint
 WHERE conrelid IN ('gd_ic_rp1'::regclass, 'gd_ic_rp2'::regclass) ORDER BY 1, 2;
SELECT attislocal FROM pg_attribute WHERE attrelid = 'gd_ic_rp1'::regclass AND attname = 'a';
DROP TABLE gd_ic_lp, gd_ic_lp6, gd_ic_rp, gd_ic_rp1, gd_ic_rp2;
