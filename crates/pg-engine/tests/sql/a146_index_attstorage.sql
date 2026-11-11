-- An index column's attstorage set apart from its table column's (test_decoding's toast
-- test: "not doable via DDL, but supported internally"), and ALTER TABLE ... SET STORAGE
-- giving the index column its table column's again (SetIndexStorageProperties).
CREATE TABLE a146_ts (id serial UNIQUE NOT NULL, k text PRIMARY KEY, c text, n int);
CREATE INDEX a146_ts_kc ON a146_ts (n, c, lower(c));
ALTER TABLE a146_ts ALTER COLUMN k SET STORAGE EXTERNAL;
ALTER TABLE a146_ts ALTER COLUMN c SET STORAGE EXTERNAL;
SELECT attrelid::regclass, attnum, attname, attstorage FROM pg_attribute
  WHERE attrelid IN ('a146_ts_pkey'::regclass, 'a146_ts_kc'::regclass) ORDER BY 1, 2;
UPDATE pg_attribute SET attstorage = 'x' WHERE attrelid = 'a146_ts_pkey'::regclass AND attname = 'k';
UPDATE pg_attribute SET attstorage = 'm' WHERE attrelid = 'a146_ts_kc'::regclass AND attnum > 1;
SELECT attrelid::regclass, attnum, attname, attstorage FROM pg_attribute
  WHERE attrelid IN ('a146_ts_pkey'::regclass, 'a146_ts_kc'::regclass) ORDER BY 1, 2;
SELECT attname, attstorage FROM pg_attribute WHERE attrelid = 'a146_ts'::regclass AND attnum > 0 ORDER BY attnum;
INSERT INTO a146_ts (k) VALUES (repeat('9876543210', 6000));
SELECT pg_column_size(k) > 2^15 FROM a146_ts;
-- SET STORAGE reaches the index columns of that table column only.
ALTER TABLE a146_ts ALTER COLUMN k SET STORAGE MAIN;
ALTER TABLE a146_ts ALTER COLUMN c SET STORAGE EXTENDED;
SELECT attrelid::regclass, attnum, attname, attstorage FROM pg_attribute
  WHERE attrelid IN ('a146_ts_pkey'::regclass, 'a146_ts_kc'::regclass) ORDER BY 1, 2;
-- A table column's, and rows the WHERE doesn't reach.
UPDATE pg_attribute SET attstorage = 'p' WHERE attrelid = 'a146_ts'::regclass AND attname = 'n';
UPDATE pg_attribute SET attstorage = 'x' WHERE attrelid = 'a146_ts_pkey'::regclass AND attnum = 99;
SELECT attname, attstorage FROM pg_attribute WHERE attrelid = 'a146_ts'::regclass AND attnum > 0 ORDER BY attnum;
REINDEX TABLE a146_ts;
SELECT attrelid::regclass, attnum, attname, attstorage FROM pg_attribute
  WHERE attrelid IN ('a146_ts_pkey'::regclass, 'a146_ts_kc'::regclass) ORDER BY 1, 2;
DROP TABLE a146_ts;
