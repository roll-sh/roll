-- System catalogs clustered (test_decoding's rewrite test), and the TOAST tables of
-- pg_description and pg_shdescription holding long comments.
CLUSTER pg_class USING pg_class_oid_index;
BEGIN;
CLUSTER pg_class USING pg_class_oid_index;
CLUSTER pg_class USING pg_class_oid_index;
ROLLBACK;
CLUSTER pg_catalog.pg_type USING pg_type_oid_index;
CLUSTER pg_database USING pg_database_oid_index;
CLUSTER pg_class USING pg_type_oid_index;
CLUSTER pg_class USING a146_no_such_index;
CLUSTER pg_class USING pg_class_relname_nsp_index;
CREATE ROLE a146_cluster_role;
SET ROLE a146_cluster_role;
CLUSTER pg_class USING pg_class_oid_index;
RESET ROLE;
DROP ROLE a146_cluster_role;
SELECT pg_relation_size((SELECT reltoastrelid FROM pg_class WHERE oid = 'pg_description'::regclass)) > 0;
CREATE TABLE a146_commented (a int);
DO $$ BEGIN
  EXECUTE format('COMMENT ON TABLE a146_commented IS %L',
    (SELECT repeat(string_agg(to_char(g.i, 'FM0000'), ''), 50) FROM generate_series(1, 500) g(i)));
END $$;
SELECT pg_relation_size((SELECT reltoastrelid FROM pg_class WHERE oid = 'pg_description'::regclass)) > 0;
CREATE ROLE a146_commented_role;
DO $$ BEGIN
  EXECUTE format('COMMENT ON ROLE a146_commented_role IS %L',
    (SELECT repeat(string_agg(to_char(g.i, 'FM0000'), ''), 50) FROM generate_series(1, 500) g(i)));
END $$;
SELECT pg_relation_size((SELECT reltoastrelid FROM pg_class WHERE oid = 'pg_shdescription'::regclass)) > 0;
SELECT length(obj_description('a146_commented'::regclass, 'pg_class'));
DROP ROLE a146_commented_role;
DROP TABLE a146_commented;
