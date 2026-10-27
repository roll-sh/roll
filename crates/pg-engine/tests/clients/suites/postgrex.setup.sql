-- Postgrex v0.22.4's test/test_helper.exs setup (its PSQL.cmd calls, in order); the suite's
-- psql stub makes those calls no-ops, since the image has no psql.
DROP DATABASE IF EXISTS postgrex_test;
DROP DATABASE IF EXISTS postgrex_test_with_schemas;
CREATE DATABASE postgrex_test TEMPLATE=template0 ENCODING='UTF8' LC_COLLATE='en_US.UTF-8' LC_CTYPE='en_US.UTF-8';
CREATE DATABASE postgrex_test_with_schemas TEMPLATE=template0 ENCODING='UTF8' LC_COLLATE='en_US.UTF-8' LC_CTYPE='en_US.UTF-8';
\c postgrex_test
DROP ROLE IF EXISTS postgrex_cleartext_pw;
DROP ROLE IF EXISTS postgrex_md5_pw;

CREATE USER postgrex_cleartext_pw WITH PASSWORD 'postgrex_cleartext_pw';
CREATE USER postgrex_md5_pw WITH PASSWORD 'postgrex_md5_pw';

DROP TABLE IF EXISTS composite1;
CREATE TABLE composite1 (a int, b text);

DROP TABLE IF EXISTS composite2;
CREATE TABLE composite2 (a int, b int, c int);

DROP TYPE IF EXISTS enum1;
CREATE TYPE enum1 AS ENUM ('elixir', 'erlang');

CREATE TABLE uniques (a int UNIQUE);
CREATE TABLE timestamps (micro timestamp, milli timestamp(3), sec timestamp(0), sec_arr timestamp(0)[]);
CREATE TABLE timestamps_stream (micro timestamp, milli timestamp(3), sec timestamp(0), sec_arr timestamp(0)[]);

DROP TABLE IF EXISTS missing_oid;
DROP TYPE IF EXISTS missing_enum;
DROP TYPE IF EXISTS missing_comp;

CREATE TABLE altering (a int2);

CREATE TABLE calendar (a timestamp without time zone, b timestamp with time zone);

DROP DOMAIN IF EXISTS points_domain;
CREATE DOMAIN points_domain AS point[] CONSTRAINT is_populated CHECK (COALESCE(array_length(VALUE, 1), 0) >= 1);

DROP DOMAIN IF EXISTS floats_domain;
CREATE DOMAIN floats_domain AS float[] CONSTRAINT is_populated CHECK (COALESCE(array_length(VALUE, 1), 0) >= 1);
DROP ROLE IF EXISTS postgrex_scram_pw;
SET password_encryption = 'scram-sha-256';
CREATE USER postgrex_scram_pw WITH PASSWORD 'postgrex_scram_pw';
CREATE PUBLICATION postgrex_example FOR ALL TABLES;
\c postgrex_test_with_schemas
DROP SCHEMA IF EXISTS test;
CREATE SCHEMA test;
CREATE EXTENSION IF NOT EXISTS hstore WITH SCHEMA test;
\c postgrex_test
CREATE EXTENSION IF NOT EXISTS hstore;
CREATE EXTENSION IF NOT EXISTS ltree;
