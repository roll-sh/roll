-- Owner dependencies of publications, subscriptions and tablespaces (pg_shdepend), and
-- DROP ROLE / REASSIGN OWNED / DROP OWNED over them.
SET client_min_messages = warning;
CREATE ROLE regress_sho_a;
CREATE ROLE regress_sho_b;
SET allow_in_place_tablespaces = true;
CREATE TABLESPACE regress_sho_spc OWNER regress_sho_a LOCATION '';
CREATE PUBLICATION regress_sho_pub;
ALTER PUBLICATION regress_sho_pub OWNER TO regress_sho_a;
CREATE SUBSCRIPTION regress_sho_sub CONNECTION 'dbname=regress_doesnotexist' PUBLICATION regress_sho_pub WITH (connect = false, slot_name = NONE);
ALTER SUBSCRIPTION regress_sho_sub OWNER TO regress_sho_a;
SELECT classid::regclass, deptype, dbid = 0 AS shared FROM pg_shdepend WHERE refobjid = 'regress_sho_a'::regrole ORDER BY classid::regclass::text;
DROP ROLE regress_sho_a;
REASSIGN OWNED BY regress_sho_a TO regress_sho_b;
SELECT spcowner::regrole FROM pg_tablespace WHERE spcname = 'regress_sho_spc';
SELECT pubowner::regrole FROM pg_publication WHERE pubname = 'regress_sho_pub';
SELECT subowner::regrole FROM pg_subscription WHERE subname = 'regress_sho_sub';
DROP ROLE regress_sho_a;
-- DROP OWNED drops the publication; the shared objects stay.
DROP OWNED BY regress_sho_b;
SELECT count(*) FROM pg_publication WHERE pubname = 'regress_sho_pub';
SELECT count(*) FROM pg_tablespace WHERE spcname = 'regress_sho_spc';
SELECT count(*) FROM pg_subscription WHERE subname = 'regress_sho_sub';
DROP ROLE regress_sho_b;
-- DROP OWNED revokes what rested on the role's grant options.
CREATE ROLE regress_sho_c;
CREATE ROLE regress_sho_d;
CREATE TABLE sho_t (a int);
GRANT ALL ON sho_t TO regress_sho_c WITH GRANT OPTION;
SET ROLE regress_sho_c;
GRANT SELECT ON sho_t TO regress_sho_d;
RESET ROLE;
GRANT INSERT ON sho_t TO regress_sho_d;
SELECT relacl FROM pg_class WHERE relname = 'sho_t';
DROP OWNED BY regress_sho_c;
SELECT relacl FROM pg_class WHERE relname = 'sho_t';
DROP TABLE sho_t;
ALTER SUBSCRIPTION regress_sho_sub OWNER TO postgres;
DROP SUBSCRIPTION regress_sho_sub;
DROP TABLESPACE regress_sho_spc;
DROP ROLE regress_sho_b;
DROP ROLE regress_sho_c;
DROP ROLE regress_sho_d;
