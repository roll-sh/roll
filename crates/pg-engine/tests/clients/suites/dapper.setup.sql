-- Dapper's test docs: database dappertest, user dappertest / dapperpass.
DROP DATABASE IF EXISTS dappertest;
DROP ROLE IF EXISTS dappertest;
CREATE USER dappertest PASSWORD 'dapperpass';
CREATE DATABASE dappertest OWNER dappertest;
