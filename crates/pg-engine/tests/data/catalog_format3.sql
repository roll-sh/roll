-- A small database of every kind of object whose maps catalog format 4 reads on demand
-- (made by a format-3 build: tests/data/catalog_format3.tar.gz).
CREATE SCHEMA s1;
CREATE TABLE s1.parent (id int PRIMARY KEY, v text DEFAULT 'x' CHECK (v <> ''), n numeric);
CREATE TABLE s1.child (extra int) INHERITS (s1.parent);
CREATE TABLE s1.part (k int NOT NULL, v text) PARTITION BY RANGE (k);
CREATE TABLE s1.part1 PARTITION OF s1.part FOR VALUES FROM (0) TO (100);
CREATE TABLE s1.ref (id serial PRIMARY KEY, p int REFERENCES s1.parent (id), big text);
CREATE INDEX ref_lower ON s1.ref (lower(big)) WHERE id > 0;
CREATE VIEW s1.v AS SELECT id, v FROM s1.parent WHERE v <> 'y';
CREATE MATERIALIZED VIEW s1.mv AS SELECT id FROM s1.parent;
CREATE SEQUENCE s1.seq START 5;
CREATE FUNCTION s1.f(a int) RETURNS int LANGUAGE sql AS 'SELECT a + 1';
CREATE TYPE s1.pair AS (a int, b text);
CREATE TABLE s1.uses (p s1.pair, ps s1.pair[]);
ALTER TABLE s1.ref ENABLE ROW LEVEL SECURITY;
CREATE POLICY pol ON s1.ref USING (id > 0);
COMMENT ON TABLE s1.parent IS 'the parent';
COMMENT ON COLUMN s1.parent.v IS 'a value';
INSERT INTO s1.parent VALUES (1, 'a', 1.5), (2, 'b', 2.5);
INSERT INTO s1.child VALUES (3, 'c', 3.5, 30);
INSERT INTO s1.part VALUES (5, 'five');
INSERT INTO s1.ref (p, big) VALUES (1, repeat('z', 5000));
SELECT nextval('s1.seq');
REFRESH MATERIALIZED VIEW s1.mv;
ANALYZE;
TRUNCATE s1.part1;
INSERT INTO s1.part VALUES (6, 'six');
DO $$ BEGIN FOR i IN 1..300 LOOP EXECUTE format('CREATE TABLE s1.t%s (id int PRIMARY KEY, a text DEFAULT %L)', i, 'd' || i); END LOOP; END $$;
