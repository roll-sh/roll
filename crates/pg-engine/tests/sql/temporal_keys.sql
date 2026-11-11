-- Temporal keys (PG 18): PRIMARY KEY / UNIQUE ... WITHOUT OVERLAPS and FOREIGN KEY ... PERIOD.
SET datestyle TO ISO, YMD;
CREATE TABLE tk_bad (valid_at daterange, CONSTRAINT tk_bad_pk PRIMARY KEY (valid_at WITHOUT OVERLAPS));
CREATE TABLE tk_bad (id int4range, CONSTRAINT tk_bad_pk PRIMARY KEY (id, valid_at WITHOUT OVERLAPS));
CREATE TABLE tk_bad (id int4range, valid_at text, CONSTRAINT tk_bad_pk PRIMARY KEY (id, valid_at WITHOUT OVERLAPS));
CREATE TABLE tk_bad (id int, valid_at daterange, CONSTRAINT tk_bad_pk PRIMARY KEY (id, valid_at WITHOUT OVERLAPS));
CREATE TABLE tk_rng (id int4range, valid_at daterange, name text, CONSTRAINT tk_rng_pk PRIMARY KEY (id, valid_at WITHOUT OVERLAPS));
SELECT conname, contype, conperiod, conkey, conexclop::regoper[], conindid::regclass FROM pg_constraint WHERE conrelid = 'tk_rng'::regclass ORDER BY conname;
SELECT indisunique, indisprimary, indisexclusion, indimmediate, indnatts FROM pg_index WHERE indexrelid = 'tk_rng_pk'::regclass;
SELECT pg_get_constraintdef(oid), pg_get_indexdef(conindid, 0, true) FROM pg_constraint WHERE conname = 'tk_rng_pk';
SELECT attname, attnotnull FROM pg_attribute WHERE attrelid = 'tk_rng'::regclass AND attnum > 0 ORDER BY attnum;
SELECT a.amname FROM pg_class c JOIN pg_am a ON a.oid = c.relam WHERE c.oid = 'tk_rng_pk'::regclass;
SELECT constraint_name, constraint_type FROM information_schema.table_constraints WHERE table_name = 'tk_rng' ORDER BY 1;
ALTER TABLE tk_rng ADD CONSTRAINT tk_rng_pk2 PRIMARY KEY (id);
ALTER TABLE tk_rng ALTER COLUMN valid_at DROP NOT NULL;
INSERT INTO tk_rng VALUES ('[1,2)', '[2018-01-01,2018-02-01)', 'a'), ('[1,2)', '[2018-02-01,2018-03-01)', 'b'), ('[2,3)', '[2018-01-01,2018-03-01)', 'c');
INSERT INTO tk_rng VALUES ('[1,2)', '[2018-01-15,2018-01-20)', 'x');
INSERT INTO tk_rng VALUES ('[3,4)', 'empty', 'x');
INSERT INTO tk_rng VALUES (NULL, '[2018-01-01,2018-02-01)', 'x');
UPDATE tk_rng SET valid_at = '[2018-01-20,2018-02-15)' WHERE name = 'b';
UPDATE tk_rng SET valid_at = 'empty' WHERE name = 'c';
SELECT * FROM tk_rng ORDER BY id, valid_at;
-- UNIQUE: NULLs pass, empty never does
CREATE TABLE tk_uq (id int4range, valid_at daterange, CONSTRAINT tk_uq_key UNIQUE (id, valid_at WITHOUT OVERLAPS));
INSERT INTO tk_uq VALUES (NULL, '[2018-01-01,2018-02-01)'), (NULL, '[2018-01-01,2018-02-01)'), ('[1,2)', NULL), ('[1,2)', NULL);
INSERT INTO tk_uq VALUES (NULL, 'empty');
SELECT conname, contype, conperiod, pg_get_constraintdef(oid) FROM pg_constraint WHERE conrelid = 'tk_uq'::regclass;
CREATE TABLE tk_uq2 (id int4range, valid_at daterange, UNIQUE (id, valid_at WITHOUT OVERLAPS));
CREATE TABLE tk_pk2 (id int4range, valid_at daterange, PRIMARY KEY (id, valid_at WITHOUT OVERLAPS));
SELECT conname, contype FROM pg_constraint WHERE conrelid IN ('tk_uq2'::regclass, 'tk_pk2'::regclass) AND contype IN ('p', 'u') ORDER BY 1;
-- adding to a table with rows
CREATE TABLE tk_add (id int4range, valid_at daterange);
INSERT INTO tk_add VALUES ('[1,2)', '[2018-01-01,2018-02-01)'), ('[1,2)', '[2018-01-15,2018-03-01)');
ALTER TABLE tk_add ADD CONSTRAINT tk_add_pk PRIMARY KEY (id, valid_at WITHOUT OVERLAPS);
DELETE FROM tk_add WHERE valid_at = '[2018-01-15,2018-03-01)';
INSERT INTO tk_add VALUES ('[2,3)', 'empty');
ALTER TABLE tk_add ADD CONSTRAINT tk_add_pk PRIMARY KEY (id, valid_at WITHOUT OVERLAPS);
DELETE FROM tk_add WHERE valid_at = 'empty';
ALTER TABLE tk_add ADD CONSTRAINT tk_add_pk PRIMARY KEY (id, valid_at WITHOUT OVERLAPS);
ALTER TABLE tk_add REPLICA IDENTITY USING INDEX tk_add_pk;
SELECT relreplident FROM pg_class WHERE oid = 'tk_add'::regclass;
CREATE TABLE tk_like (LIKE tk_add INCLUDING ALL);
SELECT conname, contype, conperiod FROM pg_constraint WHERE conrelid = 'tk_like'::regclass ORDER BY 1;
-- multiranges
CREATE TABLE tk_mr (id int4range, valid_at datemultirange, CONSTRAINT tk_mr_pk PRIMARY KEY (id, valid_at WITHOUT OVERLAPS));
INSERT INTO tk_mr VALUES ('[1,2)', '{[2018-01-01,2018-02-01),[2018-03-01,2018-04-01)}'), ('[1,2)', '{[2018-02-01,2018-03-01)}');
INSERT INTO tk_mr VALUES ('[1,2)', '{[2018-03-15,2018-03-20)}');
INSERT INTO tk_mr VALUES ('[1,2)', '{}');
-- foreign keys with PERIOD
CREATE TABLE tk_fk (id int4range, valid_at daterange, parent_id int4range,
  CONSTRAINT tk_fk_pk PRIMARY KEY (id, valid_at WITHOUT OVERLAPS),
  CONSTRAINT tk_fk_fk FOREIGN KEY (parent_id, PERIOD valid_at) REFERENCES tk_rng (id, PERIOD valid_at));
SELECT conname, contype, conperiod, confupdtype, confdeltype, pg_get_constraintdef(oid) FROM pg_constraint WHERE conname = 'tk_fk_fk';
SELECT conindid::regclass FROM pg_constraint WHERE conname = 'tk_fk_fk';
INSERT INTO tk_fk VALUES ('[10,11)', '[2018-01-05,2018-02-10)', '[1,2)');
INSERT INTO tk_fk VALUES ('[11,12)', '[2017-12-05,2018-02-10)', '[1,2)');
INSERT INTO tk_fk VALUES ('[12,13)', '[2018-01-05,2018-02-10)', '[9,10)');
INSERT INTO tk_fk VALUES ('[13,14)', '[2018-01-05,2018-02-10)', NULL);
UPDATE tk_fk SET valid_at = '[2018-01-06,2018-02-09)' WHERE id = '[10,11)';
UPDATE tk_fk SET valid_at = '[2018-01-01,2018-05-01)' WHERE id = '[10,11)';
UPDATE tk_rng SET valid_at = '[2017-01-01,2017-02-01)' WHERE name = 'a';
UPDATE tk_rng SET valid_at = '[2017-12-01,2018-02-01)' WHERE name = 'a';
DELETE FROM tk_rng WHERE name = 'b';
UPDATE tk_rng SET id = '[5,6)' WHERE name = 'b';
BEGIN;
ALTER TABLE tk_fk ALTER CONSTRAINT tk_fk_fk DEFERRABLE INITIALLY DEFERRED;
DELETE FROM tk_rng WHERE name = 'b';
INSERT INTO tk_rng VALUES ('[1,2)', '[2018-01-20,2018-02-15)', 'b2');
COMMIT;
BEGIN;
ALTER TABLE tk_fk ALTER CONSTRAINT tk_fk_fk DEFERRABLE INITIALLY DEFERRED;
DELETE FROM tk_rng WHERE name = 'b2';
COMMIT;
SELECT * FROM tk_rng ORDER BY id, valid_at;
-- referenced columns inferred, actions, PERIOD mismatches
ALTER TABLE tk_fk DROP CONSTRAINT tk_fk_fk;
ALTER TABLE tk_fk ADD CONSTRAINT tk_fk_fk2 FOREIGN KEY (parent_id, PERIOD valid_at) REFERENCES tk_rng;
ALTER TABLE tk_fk ADD CONSTRAINT tk_fk_fk3 FOREIGN KEY (parent_id, valid_at) REFERENCES tk_rng;
ALTER TABLE tk_fk ADD CONSTRAINT tk_fk_fk3 FOREIGN KEY (parent_id, valid_at) REFERENCES tk_rng (id, valid_at);
ALTER TABLE tk_fk ADD CONSTRAINT tk_fk_fk3 FOREIGN KEY (parent_id, PERIOD valid_at) REFERENCES tk_rng (id, valid_at);
ALTER TABLE tk_fk ADD CONSTRAINT tk_fk_fk3 FOREIGN KEY (parent_id, valid_at) REFERENCES tk_rng (id, PERIOD valid_at);
ALTER TABLE tk_fk ADD CONSTRAINT tk_fk_fk3 FOREIGN KEY (parent_id, PERIOD parent_id) REFERENCES tk_rng (id, PERIOD id);
ALTER TABLE tk_fk ADD CONSTRAINT tk_fk_fk3 FOREIGN KEY (parent_id, PERIOD valid_at) REFERENCES tk_rng ON DELETE CASCADE;
ALTER TABLE tk_fk ADD CONSTRAINT tk_fk_fk3 FOREIGN KEY (parent_id, PERIOD valid_at) REFERENCES tk_rng ON UPDATE RESTRICT;
ALTER TABLE tk_fk ADD CONSTRAINT tk_fk_fk3 FOREIGN KEY (parent_id, PERIOD valid_at) REFERENCES tk_rng ON DELETE SET NULL;
CREATE TABLE tk_ts (id int4range, valid_at tsrange, parent_id int4range);
ALTER TABLE tk_ts ADD CONSTRAINT tk_ts_fk FOREIGN KEY (parent_id, PERIOD valid_at) REFERENCES tk_rng (id, PERIOD valid_at);
INSERT INTO tk_fk VALUES ('[14,15)', '[2030-01-01,2030-02-01)', '[1,2)');
ALTER TABLE tk_fk ADD CONSTRAINT tk_fk_fk4 FOREIGN KEY (parent_id, PERIOD valid_at) REFERENCES tk_rng (id, PERIOD valid_at);
-- partitioned
CREATE TABLE tk_part (id int4range, valid_at daterange, CONSTRAINT tk_part_pk PRIMARY KEY (id, valid_at WITHOUT OVERLAPS)) PARTITION BY LIST (id);
CREATE TABLE tk_part1 PARTITION OF tk_part FOR VALUES IN ('[1,2)', '[3,4)');
CREATE TABLE tk_part2 PARTITION OF tk_part FOR VALUES IN ('[2,3)');
SELECT c.conrelid::regclass, c.conname, c.contype, c.conperiod FROM pg_constraint c WHERE c.conrelid IN ('tk_part1'::regclass, 'tk_part2'::regclass) AND contype IN ('p', 'u', 'x') ORDER BY 1;
INSERT INTO tk_part VALUES ('[1,2)', '[2000-01-01,2000-02-01)'), ('[2,3)', '[2000-01-01,2000-02-01)');
INSERT INTO tk_part VALUES ('[1,2)', '[2000-01-15,2000-03-01)');
CREATE TABLE tk_part_fk (id int4range, valid_at daterange, parent_id int4range,
  CONSTRAINT tk_part_fk_fk FOREIGN KEY (parent_id, PERIOD valid_at) REFERENCES tk_part (id, PERIOD valid_at));
INSERT INTO tk_part_fk VALUES ('[1,2)', '[2000-01-10,2000-01-20)', '[1,2)');
INSERT INTO tk_part_fk VALUES ('[1,2)', '[2000-01-10,2000-03-20)', '[1,2)');
DELETE FROM tk_part WHERE id = '[1,2)';
DELETE FROM tk_part WHERE id = '[2,3)';
DROP TABLE tk_part_fk, tk_part, tk_ts, tk_fk, tk_mr, tk_like, tk_add, tk_pk2, tk_uq2, tk_uq, tk_rng;
