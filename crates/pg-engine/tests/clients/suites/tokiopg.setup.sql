-- rust-postgres's docker/sql_setup.sh, in its own database (the tests' dbname is patched to
-- a51_tokiopg). Its pg_hba gives pass_user `password`, md5_user `md5` and ssl_user `hostssl
-- trust`; here every role authenticates with SCRAM, so the suite skips the plain / md5 tests.
DROP DATABASE IF EXISTS a51_tokiopg;
CREATE DATABASE a51_tokiopg;
DROP ROLE IF EXISTS pass_user, md5_user, scram_user, ssl_user;
SET password_encryption TO 'scram-sha-256';
CREATE ROLE pass_user PASSWORD 'password' LOGIN;
CREATE ROLE md5_user PASSWORD 'password' LOGIN;
CREATE ROLE scram_user PASSWORD 'password' LOGIN;
CREATE ROLE ssl_user LOGIN;
\c a51_tokiopg
CREATE EXTENSION hstore;
CREATE EXTENSION citext;
CREATE EXTENSION ltree;
