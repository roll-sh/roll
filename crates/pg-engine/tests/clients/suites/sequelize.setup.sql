-- Sequelize's CI PostgreSQL service: superuser sequelize_test / sequelize_test, database
-- sequelize_test, with the extensions its image (sushantdhiman/postgres) ships, minus PostGIS.
DROP DATABASE IF EXISTS sequelize_test;
DROP ROLE IF EXISTS sequelize_test;
CREATE ROLE sequelize_test SUPERUSER LOGIN PASSWORD 'sequelize_test';
CREATE DATABASE sequelize_test OWNER sequelize_test;
\c sequelize_test
CREATE EXTENSION IF NOT EXISTS hstore;
CREATE EXTENSION IF NOT EXISTS citext;
