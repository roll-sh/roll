-- pgx's testsetup/postgresql_setup.sql, in its own database (and the roles dropped first).
DROP DATABASE IF EXISTS pgx_test;
CREATE DATABASE pgx_test;
\c pgx_test
create extension hstore;
create extension ltree;
create domain uint64 as numeric(20,0);
DROP ROLE IF EXISTS pgx_ssl, pgx_sslcert, pgx_md5, pgx_pw, pgx_scram, " tricky, ' } "" \\ test user ";
create user pgx_ssl with superuser PASSWORD 'secret';
create user pgx_sslcert with superuser PASSWORD 'secret';
set password_encryption = md5;
create user pgx_md5 with superuser PASSWORD 'secret';
set password_encryption = 'scram-sha-256';
create user pgx_pw with superuser PASSWORD 'secret';
create user pgx_scram with superuser PASSWORD 'secret';
create user " tricky, ' } "" \\ test user " superuser password 'secret';
