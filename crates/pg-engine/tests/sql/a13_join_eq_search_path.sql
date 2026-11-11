-- A join's hash keys rebuilt as clauses (a left join under a filter above outer joins) keep
-- the `=` they were made from: not whatever `=` the search path finds now, nor one an
-- earlier statement's search path found.
CREATE TABLE tk (unique1 int, unique2 int, thousand int, twothousand int);
INSERT INTO tk SELECT (i * 7919) % 10000, i, ((i * 7919) % 10000) % 1000, ((i * 7919) % 10000) % 2000
  FROM generate_series(0, 9999) i;
CREATE INDEX tk_unique2 ON tk (unique2);
CREATE INDEX tk_thous ON tk (thousand);
VACUUM ANALYZE tk;
CREATE SCHEMA s;
CREATE FUNCTION s.always(int, int) RETURNS bool LANGUAGE plpgsql IMMUTABLE AS $$ BEGIN RETURN true; END $$;
CREATE OPERATOR s.= (leftarg = int, rightarg = int, function = s.always);
-- Plain `=`: the built-in one.
SELECT a.unique1, b.unique1, c.unique1, coalesce(b.twothousand, a.twothousand)
  FROM tk a LEFT JOIN tk b ON b.thousand = a.unique1
  LEFT JOIN tk c ON c.unique2 = coalesce(b.twothousand, a.twothousand)
  WHERE a.unique2 < 10 AND coalesce(b.twothousand, a.twothousand) = 44;
-- With s first on the path, the query names pg_catalog's `=` explicitly.
SET search_path = s, pg_catalog, public;
SELECT a.unique1, b.unique1, c.unique1, coalesce(b.twothousand, a.twothousand)
  FROM tk a LEFT JOIN tk b ON b.thousand OPERATOR(pg_catalog.=) a.unique1
  LEFT JOIN tk c ON c.unique2 OPERATOR(pg_catalog.=) coalesce(b.twothousand, a.twothousand)
  WHERE a.unique2 OPERATOR(pg_catalog.<) 10
    AND coalesce(b.twothousand, a.twothousand) OPERATOR(pg_catalog.=) 44;
SELECT count(*) FROM tk a JOIN tk b ON a.unique1 OPERATOR(pg_catalog.=) b.unique2;
RESET search_path;
-- Back on the default path.
SELECT a.unique1, b.unique1, c.unique1, coalesce(b.twothousand, a.twothousand)
  FROM tk a LEFT JOIN tk b ON b.thousand = a.unique1
  LEFT JOIN tk c ON c.unique2 = coalesce(b.twothousand, a.twothousand)
  WHERE a.unique2 < 10 AND coalesce(b.twothousand, a.twothousand) = 44;
SELECT count(*) FROM tk a JOIN tk b ON a.unique1 = b.unique2;
