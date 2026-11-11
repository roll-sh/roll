-- Round 17 (agent 37): stored expressions name their objects for good, as Postgres's trees
-- (oids) do: a default, generation expression, CHECK, index expression or predicate, trigger
-- WHEN, policy, statistics expression or domain CHECK made under one search_path works, and
-- prints qualified, under another (pg_dump runs with search_path = ''). A generation
-- expression's tableoid is the column, not the table's oid.
CREATE SCHEMA a37s;
SET search_path = a37s, public;
CREATE FUNCTION a37f(int) RETURNS int LANGUAGE sql IMMUTABLE AS 'SELECT $1';
CREATE SEQUENCE a37seq;
CREATE TYPE a37e AS ENUM ('x', 'y');
CREATE DOMAIN a37d AS int CHECK (VALUE > a37f(0));
CREATE TABLE a37t (
  a int DEFAULT a37f(1) CHECK (a < a37f(100)),
  b int DEFAULT nextval('a37seq'),
  c a37e DEFAULT 'x',
  d regclass DEFAULT 'a37t',
  e bool GENERATED ALWAYS AS (tableoid = 'a37t'::regclass) VIRTUAL,
  f oid GENERATED ALWAYS AS (tableoid) VIRTUAL,
  g int GENERATED ALWAYS AS (a37f(a) + 1) STORED,
  h a37d
);
ALTER TABLE a37t ADD CONSTRAINT a37c CHECK (b > a37f(-1));
CREATE INDEX a37i ON a37t (a37f(a)) WHERE a37f(b) > 0;
CREATE STATISTICS a37st ON (a37f(a)), b FROM a37t;
ALTER TABLE a37t ENABLE ROW LEVEL SECURITY;
CREATE POLICY a37p ON a37t USING (a37f(a) > 0) WITH CHECK (c = 'x'::a37e);
CREATE FUNCTION a37trf() RETURNS trigger LANGUAGE plpgsql AS 'BEGIN RETURN NEW; END';
CREATE TRIGGER a37tr BEFORE INSERT ON a37t FOR EACH ROW WHEN (NEW.a > a37f(0))
  EXECUTE FUNCTION a37trf();
SELECT adnum, pg_get_expr(adbin, adrelid) FROM pg_attrdef WHERE adrelid = 'a37t'::regclass
  ORDER BY 1;
SET search_path = '';
SELECT adnum, pg_get_expr(adbin, adrelid) FROM pg_attrdef WHERE adrelid = 'a37s.a37t'::regclass
  ORDER BY 1;
SELECT conname, pg_get_constraintdef(oid) FROM pg_constraint
  WHERE conrelid = 'a37s.a37t'::regclass OR contypid = 'a37s.a37d'::regtype ORDER BY 1;
SELECT pg_get_indexdef('a37s.a37i'::regclass);
SELECT pg_get_statisticsobjdef(oid) FROM pg_statistic_ext WHERE stxname = 'a37st';
SELECT polname, pg_get_expr(polqual, polrelid), pg_get_expr(polwithcheck, polrelid)
  FROM pg_policy WHERE polname = 'a37p';
SELECT pg_get_triggerdef(oid) FROM pg_trigger WHERE tgname = 'a37tr';
INSERT INTO a37s.a37t (h) VALUES (5);
SELECT a, b, c, d, e, f = 'a37s.a37t'::regclass AS f, g, h FROM a37s.a37t;
INSERT INTO a37s.a37t (a) VALUES (500);
SET search_path = public;
INSERT INTO a37s.a37t (a, h) VALUES (2, 3);
SELECT a, b, c, d, e, g, h FROM a37s.a37t ORDER BY a;
RESET search_path;
DROP SCHEMA a37s CASCADE;
