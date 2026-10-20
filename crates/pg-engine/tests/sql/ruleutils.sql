-- How stored expressions print (pg_get_expr, pg_get_constraintdef, pg_get_indexdef):
-- introspection tools parse this text.
CREATE TYPE mood AS ENUM ('a', 'b');
CREATE TABLE ru (
  id serial PRIMARY KEY,
  i2 int2 DEFAULT 0,
  i8 int8 DEFAULT -5,
  n numeric DEFAULT 1.5 CHECK (n > 0),
  t text DEFAULT 'x',
  vc varchar(10) DEFAULT 'y',
  c char(3) DEFAULT 'z',
  j jsonb DEFAULT '{}',
  ar int[] DEFAULT '{1,2}',
  ta text[] DEFAULT ARRAY['a', 'b'],
  e mood DEFAULT 'a',
  neg int DEFAULT -1,
  ex int DEFAULT (1 + 2) * 3,
  lo text DEFAULT lower('ABC'),
  iv interval DEFAULT '1 day',
  who name DEFAULT CURRENT_USER,
  ts timestamptz DEFAULT CURRENT_TIMESTAMP,
  d date DEFAULT CURRENT_DATE,
  ts2 timestamp DEFAULT now(),
  b bool DEFAULT true,
  f float8 DEFAULT 2.5,
  CHECK (t LIKE '%x%' AND t IS NOT NULL),
  CHECK (i8 BETWEEN 1 AND 10),
  CHECK (COALESCE(t, '') <> 'q'),
  CHECK (CASE WHEN b THEN i2 > 0 END),
  CHECK (NOT b OR i8 = 5),
  CHECK (j ? 'k' OR j = '{}'),
  CHECK (f::int > 0),
  CHECK (vc <> ''),
  CHECK (i2 IN (1, 2) AND t IS NOT NULL),
  CHECK (i2 NOT IN (3)),
  CHECK (-i2 < 5 AND (i2 + 1) * 2 > 0 AND i2 - (1 - 2) > 0),
  CHECK (CASE i2 WHEN 1 THEN true ELSE false END),
  CHECK (ar[1] > 0 AND (i2 IS NOT DISTINCT FROM 1)),
  CHECK (greatest(i2, 2) > nullif(i2, 3)),
  CHECK (t || 'x' <> 'y' AND length(t) > 1 AND (i2::text)::int > 0),
  CHECK (b IS NOT TRUE OR i2 * 2 + 1 > i2 - 1 - 1)
);
SELECT a.attname, pg_get_expr(d.adbin, d.adrelid) AS plain, pg_get_expr(d.adbin, d.adrelid, true) AS pretty
FROM pg_attrdef d JOIN pg_attribute a ON a.attrelid = d.adrelid AND a.attnum = d.adnum
WHERE d.adrelid = 'ru'::regclass ORDER BY d.adnum;
SELECT column_name, column_default FROM information_schema.columns WHERE table_name = 'ru' ORDER BY ordinal_position;
SELECT pg_get_constraintdef(oid, true) AS pretty, pg_get_constraintdef(oid) AS plain
FROM pg_constraint WHERE conrelid = 'ru'::regclass AND contype = 'c' ORDER BY pg_get_constraintdef(oid, true);
CREATE INDEX ru_partial ON ru (i8) WHERE b AND n > 1;
CREATE INDEX ru_expr ON ru ((i8 + 1), t DESC NULLS LAST);
CREATE INDEX ru_func ON ru (lower(t), coalesce(vc, 'q'));
SELECT pg_get_indexdef(i.indexrelid, 0, true), pg_get_indexdef(i.indexrelid)
FROM pg_index i JOIN pg_class c ON c.oid = i.indexrelid WHERE i.indrelid = 'ru'::regclass ORDER BY c.relname;
SELECT pg_get_indexdef('ru_expr'::regclass, 1, true), pg_get_indexdef('ru_expr'::regclass, 2, true), pg_get_indexdef('ru_func'::regclass, 1, false);
SELECT indexname, indexdef FROM pg_indexes WHERE tablename = 'ru' ORDER BY indexname;
SELECT pg_get_expr(i.indexprs, i.indrelid), pg_get_expr(i.indpred, i.indrelid, true)
FROM pg_index i JOIN pg_class c ON c.oid = i.indexrelid WHERE i.indrelid = 'ru'::regclass ORDER BY c.relname;
DROP TABLE ru;
DROP TYPE mood;
