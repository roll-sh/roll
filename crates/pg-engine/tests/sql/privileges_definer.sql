-- Roles are cluster-wide in Postgres: clean up first and at the end.
DROP ROLE IF EXISTS pd_owner, pd_user;
CREATE ROLE pd_owner;
CREATE ROLE pd_user;
GRANT CREATE ON SCHEMA public TO pd_owner;
SET ROLE pd_owner;
CREATE TABLE pd_t (id int PRIMARY KEY, v text);
INSERT INTO pd_t VALUES (1, 'a'), (2, 'b');
CREATE FUNCTION pd_count_invoker() RETURNS bigint LANGUAGE sql AS 'SELECT count(*) FROM pd_t';
CREATE FUNCTION pd_count_definer() RETURNS bigint LANGUAGE sql SECURITY DEFINER AS 'SELECT count(*) FROM pd_t';
CREATE FUNCTION pd_pl_definer() RETURNS text LANGUAGE plpgsql SECURITY DEFINER AS $$
BEGIN
  INSERT INTO pd_t VALUES ((SELECT max(id) + 1 FROM pd_t), current_user);
  RETURN current_user || '/' || session_user;
END $$;
CREATE FUNCTION pd_pl_invoker() RETURNS text LANGUAGE plpgsql AS $$
BEGIN
  RETURN current_user;
END $$;
CREATE FUNCTION pd_whoami() RETURNS text LANGUAGE sql SECURITY DEFINER AS 'SELECT current_user::text';
RESET ROLE;
SELECT proname, prosecdef, proowner::regrole FROM pg_proc WHERE proname LIKE 'pd\_%' ORDER BY 1;
SET ROLE pd_user;
SELECT pd_count_invoker();
SELECT pd_count_definer();
SELECT pd_pl_definer();
SELECT pd_pl_invoker();
SELECT current_user;
SELECT * FROM pd_t;
SELECT pd_whoami();
RESET ROLE;
SELECT id, v FROM pd_t ORDER BY id;
-- LOCK needs a privilege matching the mode
GRANT SELECT ON pd_t TO pd_user;
SET ROLE pd_user;
BEGIN;
LOCK TABLE pd_t IN ACCESS SHARE MODE;
LOCK TABLE pd_t IN ROW EXCLUSIVE MODE;
ROLLBACK;
BEGIN;
LOCK TABLE pd_t;
ROLLBACK;
RESET ROLE;
GRANT UPDATE ON pd_t TO pd_user;
SET ROLE pd_user;
BEGIN;
LOCK TABLE pd_t IN EXCLUSIVE MODE;
ROLLBACK;
-- TRIGGER and REFERENCES privileges
CREATE TABLE pd_child (id int REFERENCES pd_t);
RESET ROLE;
GRANT CREATE ON SCHEMA public TO pd_user;
SET ROLE pd_user;
CREATE TABLE pd_child (id int REFERENCES pd_t);
CREATE TABLE pd_child (id int);
ALTER TABLE pd_child ADD FOREIGN KEY (id) REFERENCES pd_t;
CREATE FUNCTION pd_trig() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END $$;
CREATE TRIGGER pd_tr BEFORE INSERT ON pd_t FOR EACH ROW EXECUTE FUNCTION pd_trig();
RESET ROLE;
GRANT REFERENCES (id) ON pd_t TO pd_user;
GRANT TRIGGER ON pd_t TO pd_user;
SET ROLE pd_user;
ALTER TABLE pd_child ADD FOREIGN KEY (id) REFERENCES pd_t;
CREATE TRIGGER pd_tr BEFORE INSERT ON pd_t FOR EACH ROW EXECUTE FUNCTION pd_trig();
SELECT tgname, tgrelid::regclass FROM pg_trigger WHERE tgname = 'pd_tr';
DROP TRIGGER pd_tr ON pd_t;
RESET ROLE;
DROP TRIGGER pd_tr ON pd_t;
-- Temporary tables need TEMP on the database (PUBLIC has it by default)
SET ROLE pd_user;
CREATE TEMP TABLE pd_tmp (a int);
INSERT INTO pd_tmp VALUES (1);
SELECT * FROM pd_tmp;
DROP TABLE pd_tmp;
RESET ROLE;
-- Types and domains
SET ROLE pd_owner;
CREATE TYPE pd_mood AS ENUM ('sad', 'ok');
CREATE DOMAIN pd_pos AS int CHECK (VALUE > 0);
RESET ROLE;
SET ROLE pd_user;
ALTER TYPE pd_mood ADD VALUE 'happy';
DROP TYPE pd_mood;
DROP DOMAIN pd_pos;
COMMENT ON TYPE pd_mood IS 'x';
SELECT 'ok'::pd_mood, 5::pd_pos;
RESET ROLE;
REVOKE USAGE ON TYPE pd_mood FROM PUBLIC;
REVOKE USAGE ON DOMAIN pd_pos FROM PUBLIC;
SELECT typname, typacl FROM pg_type WHERE typname IN ('pd_mood', 'pd_pos') ORDER BY 1;
SELECT grantor, grantee, object_name, object_type, privilege_type FROM information_schema.usage_privileges WHERE object_schema = 'public' ORDER BY 3, 2;
SELECT grantor, grantee, udt_name, privilege_type FROM information_schema.udt_privileges WHERE udt_schema = 'public' ORDER BY 3, 2;
-- Cleanup
DROP OWNED BY pd_owner, pd_user CASCADE;
REVOKE CREATE ON SCHEMA public FROM pd_owner, pd_user;
DROP ROLE pd_owner, pd_user;
