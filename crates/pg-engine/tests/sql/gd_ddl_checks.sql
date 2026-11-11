-- CREATE TABLE / partition key / trigger checks and their error positions.
CREATE TYPE gd_dc_comp AS (u unknown);
CREATE TYPE gd_dc_comp AS (r record);
CREATE TABLE gd_dc_d1 (id int DEFAULT (id));
CREATE TABLE gd_dc_d1 (id int DEFAULT (avg(nope)));
CREATE TABLE gd_dc_d1 (a int DEFAULT (SELECT 1));
CREATE TABLE gd_dc_d1 (a int DEFAULT (generate_series(1, 3)));
CREATE TABLE gd_dc_d1 (a int CHECK (a > (SELECT 1)));
CREATE UNLOGGED TABLE pg_temp.gd_dc_u (a int);
CREATE TABLE gd_dc_inh (a int);
CREATE TABLE gd_dc_p (a int) INHERITS (gd_dc_nope) PARTITION BY LIST (a);
-- partition keys: expression checks carry no position
CREATE FUNCTION gd_dc_srf(int) RETURNS SETOF int LANGUAGE sql IMMUTABLE AS 'SELECT 1';
CREATE FUNCTION gd_dc_plus(int) RETURNS int LANGUAGE sql AS 'SELECT $1 + 1';
CREATE FUNCTION gd_dc_vol(int) RETURNS int LANGUAGE sql AS 'SELECT $1 + random()::int';
CREATE TABLE gd_dc_p (a int) PARTITION BY RANGE (gd_dc_srf(a));
CREATE TABLE gd_dc_p (a int) PARTITION BY RANGE ((avg(a)));
CREATE TABLE gd_dc_p (a int) PARTITION BY LIST ((a = (SELECT 1)));
CREATE TABLE gd_dc_p (a int) PARTITION BY RANGE ((42));
CREATE TABLE gd_dc_p (a int, b int) PARTITION BY RANGE (((a, b)));
CREATE TABLE gd_dc_p (a int, b int) PARTITION BY RANGE (a, ('unknown'));
CREATE TABLE gd_dc_p (a int) PARTITION BY RANGE (gd_dc_vol(a));
CREATE TABLE gd_dc_p (a point) PARTITION BY LIST (a);
-- an inlined SQL function is as immutable as its body; oid_ops takes an int
CREATE TABLE gd_dc_p (a int, b int, c text, d text)
  PARTITION BY RANGE (a oid_ops, gd_dc_plus(b), c COLLATE "default", d COLLATE "C");
SELECT pg_get_partkeydef('gd_dc_p'::regclass);
DROP FUNCTION gd_dc_plus(int);
DROP TABLE gd_dc_p;
-- the table's own row type in its key
CREATE TABLE gd_dc_p (a int, b int) PARTITION BY LIST ((row(a, b)::gd_dc_p));
CREATE TABLE gd_dc_p1 PARTITION OF gd_dc_p FOR VALUES IN ('(1,2)');
INSERT INTO gd_dc_p VALUES (1, 2);
SELECT * FROM gd_dc_p;
DROP TABLE gd_dc_p;
-- partition bounds
CREATE TABLE gd_dc_l (a int) PARTITION BY LIST (a);
CREATE TABLE gd_dc_l1 PARTITION OF gd_dc_l FOR VALUES IN (sum(a));
CREATE TABLE gd_dc_l1 PARTITION OF gd_dc_l FOR VALUES IN (sum(1));
CREATE TABLE gd_dc_l1 PARTITION OF gd_dc_l FOR VALUES IN (generate_series(1, 2));
CREATE TABLE gd_dc_l1 PARTITION OF gd_dc_l FOR VALUES IN ((SELECT 1));
CREATE TABLE gd_dc_r (a int, b int) PARTITION BY RANGE (a, b);
CREATE TABLE gd_dc_r1 PARTITION OF gd_dc_r FOR VALUES FROM (null, 1) TO (2, 3);
CREATE TABLE gd_dc_r0 PARTITION OF gd_dc_r FOR VALUES FROM (0, minvalue) TO (0, maxvalue);
CREATE TABLE gd_dc_r10 PARTITION OF gd_dc_r FOR VALUES FROM (1, minvalue) TO (1, 1);
CREATE TABLE gd_dc_r11 PARTITION OF gd_dc_r FOR VALUES FROM (1, 1) TO (1, 10);
CREATE TABLE gd_dc_r12 PARTITION OF gd_dc_r FOR VALUES FROM (1, 10) TO (1, maxvalue);
CREATE TABLE gd_dc_rx PARTITION OF gd_dc_r FOR VALUES FROM (0, minvalue) TO (0, 1);
CREATE TABLE gd_dc_rx PARTITION OF gd_dc_r FOR VALUES FROM (1, minvalue) TO (1, maxvalue);
CREATE TABLE gd_dc_rx PARTITION OF gd_dc_r FOR VALUES FROM (1, 10) TO (1, 20);
CREATE TABLE gd_dc_rx PARTITION OF gd_dc_r FOR VALUES FROM (1, 5) TO (1, 2);
CREATE TABLE gd_dc_rx PARTITION OF gd_dc_r FOR VALUES FROM (-1, 0) TO (0, 5);
-- a partition's column list; its local CHECK merges into the inherited one
CREATE TABLE gd_dc_pt (a text, b int NOT NULL DEFAULT 0, CONSTRAINT gd_dc_ck CHECK (length(a) > 0))
  PARTITION BY LIST (a);
CREATE TABLE gd_dc_pt_b PARTITION OF gd_dc_pt (b NOT NULL, b DEFAULT 1) FOR VALUES IN ('b');
CREATE TABLE gd_dc_pt_b PARTITION OF gd_dc_pt (CONSTRAINT gd_dc_ck CHECK (length(a) > 0),
  CONSTRAINT gd_dc_ck2 CHECK (b >= 0)) FOR VALUES IN ('b');
ALTER TABLE gd_dc_pt ADD CONSTRAINT gd_dc_ck2 CHECK (b >= 0);
SELECT conname, conislocal, coninhcount FROM pg_constraint
 WHERE conrelid = 'gd_dc_pt_b'::regclass AND contype = 'c' ORDER BY 1;
ALTER TABLE gd_dc_pt DROP CONSTRAINT gd_dc_ck, DROP CONSTRAINT gd_dc_ck2;
SELECT count(*) FROM pg_constraint WHERE conrelid = 'gd_dc_pt_b'::regclass AND contype = 'c';
-- a table its own trigger is changing can't be altered
CREATE TABLE gd_dc_tt (a int, b int);
CREATE FUNCTION gd_dc_tf() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN ALTER TABLE gd_dc_tt ADD PRIMARY KEY (a); RETURN NEW; END $$;
CREATE TRIGGER gd_dc_tg BEFORE INSERT ON gd_dc_tt FOR EACH ROW EXECUTE FUNCTION gd_dc_tf();
INSERT INTO gd_dc_tt VALUES (1, 2);
CREATE OR REPLACE FUNCTION gd_dc_tf() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN CREATE INDEX ON gd_dc_tt (b); RETURN NEW; END $$;
INSERT INTO gd_dc_tt VALUES (1, 2);
CREATE OR REPLACE FUNCTION gd_dc_tf() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN EXECUTE 'create table gd_dc_l9 partition of gd_dc_l for values in (9)'; RETURN NULL; END $$;
CREATE TRIGGER gd_dc_tg2 BEFORE INSERT ON gd_dc_l FOR EACH STATEMENT EXECUTE FUNCTION gd_dc_tf();
INSERT INTO gd_dc_l VALUES (9);
-- trigger WHEN conditions
CREATE FUNCTION gd_dc_nf() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NULL; END $$;
CREATE TRIGGER gd_dc_w1 BEFORE INSERT ON gd_dc_tt FOR EACH STATEMENT WHEN (true) EXECUTE FUNCTION gd_dc_nf();
CREATE TRIGGER gd_dc_w2 BEFORE INSERT ON gd_dc_tt FOR EACH STATEMENT WHEN (new.a > 0) EXECUTE FUNCTION gd_dc_nf();
CREATE TRIGGER gd_dc_w3 BEFORE INSERT OR UPDATE ON gd_dc_tt FOR EACH ROW WHEN (OLD.a <> NEW.a) EXECUTE FUNCTION gd_dc_nf();
CREATE TRIGGER gd_dc_w3 BEFORE DELETE OR UPDATE ON gd_dc_tt FOR EACH ROW WHEN (OLD.a <> NEW.a) EXECUTE FUNCTION gd_dc_nf();
CREATE TRIGGER gd_dc_w3 BEFORE INSERT ON gd_dc_tt FOR EACH ROW WHEN (NEW.tableoid <> 0) EXECUTE FUNCTION gd_dc_nf();
CREATE TRIGGER gd_dc_w3 AFTER INSERT ON gd_dc_tt FOR EACH ROW WHEN (NEW.tableoid <> 0) EXECUTE FUNCTION gd_dc_nf();
CREATE TRIGGER gd_dc_w4 BEFORE UPDATE OF a, a ON gd_dc_tt FOR EACH ROW EXECUTE FUNCTION gd_dc_nf();
CREATE TRIGGER gd_dc_w4 BEFORE UPDATE OF a ON gd_dc_tt FOR EACH ROW WHEN (OLD.* IS DISTINCT FROM NEW.*) EXECUTE FUNCTION gd_dc_nf();
SELECT pg_get_triggerdef(oid, true) FROM pg_trigger WHERE tgname = 'gd_dc_w4';
DROP TABLE gd_dc_l, gd_dc_r, gd_dc_pt, gd_dc_tt, gd_dc_inh;
DROP FUNCTION gd_dc_srf(int), gd_dc_vol(int), gd_dc_tf(), gd_dc_nf();
