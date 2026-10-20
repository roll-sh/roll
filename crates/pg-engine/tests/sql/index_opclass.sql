CREATE TABLE ti (a text, b int, c varchar(10), d jsonb, e int[], n name, f text COLLATE "C");
CREATE INDEX ti_a ON ti (a text_pattern_ops);
CREATE INDEX ti_a2 ON ti (a COLLATE "C" DESC);
CREATE INDEX ti_lower ON ti (lower(a) varchar_pattern_ops);
CREATE INDEX ti_b ON ti (b int4_ops);
CREATE INDEX ti_c ON ti (c);
CREATE INDEX ti_c2 ON ti (c varchar_pattern_ops NULLS FIRST);
CREATE INDEX ti_d ON ti USING gin (d);
CREATE INDEX ti_d2 ON ti USING gin (d jsonb_path_ops);
CREATE INDEX ti_e ON ti USING gin (e);
CREATE INDEX ti_n ON ti (n);
CREATE INDEX ti_f ON ti (f);
CREATE INDEX ti_f2 ON ti (f COLLATE "C");
CREATE INDEX ti_f3 ON ti (f COLLATE "default");
CREATE INDEX ti_expr ON ti ((b + 1), upper(a));
CREATE INDEX ti_h ON ti USING hash (a);
CREATE INDEX ON ti (a foo_ops);
CREATE INDEX ON ti (b text_ops);
CREATE INDEX ON ti (a COLLATE "xx");
CREATE INDEX ON ti (b COLLATE "C");
CREATE INDEX ON ti USING gin (b);
SELECT c.relname, pg_get_indexdef(i.indexrelid), i.indclass::text, i.indcollation::text
FROM pg_index i JOIN pg_class c ON c.oid = i.indexrelid WHERE i.indrelid = 'ti'::regclass ORDER BY 1;
SELECT c.relname, k.n, pg_get_indexdef(i.indexrelid, k.n, true)
FROM pg_index i JOIN pg_class c ON c.oid = i.indexrelid, generate_series(1, i.indnatts) k(n)
WHERE i.indrelid = 'ti'::regclass ORDER BY 1, 2;
SELECT c.relname, opc.opcname, opc.opcdefault, opf.opfname, am.amname
FROM pg_index i JOIN pg_class c ON c.oid = i.indexrelid
JOIN pg_opclass opc ON opc.oid = ANY(i.indclass)
JOIN pg_opfamily opf ON opf.oid = opc.opcfamily
JOIN pg_am am ON am.oid = opc.opcmethod
WHERE i.indrelid = 'ti'::regclass ORDER BY 1, 2;
SELECT attname, attcollation FROM pg_attribute WHERE attrelid = 'ti'::regclass AND attnum > 0 ORDER BY attnum;
SELECT count(*), count(*) FILTER (WHERE opcdefault) FROM pg_opclass;
SELECT count(*) FROM pg_opfamily;
INSERT INTO ti (a, b) VALUES ('abc', 1), ('abd', 2);
SELECT a FROM ti WHERE a LIKE 'ab%' ORDER BY a;
