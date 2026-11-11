-- Hibernate ORM's PostgreSQL test databases (docker_db.sh postgresql_setup): the superuser
-- hibernate_orm_test (the compose file's POSTGRES_USER) and one database per test fork,
-- hibernate_orm_test_1..4 (the suite runs 4 forks). No pgvector / PostGIS here (only the vector
-- and spatial modules use them; they are not run).
DROP DATABASE IF EXISTS hibernate_orm_test_1;
DROP DATABASE IF EXISTS hibernate_orm_test_2;
DROP DATABASE IF EXISTS hibernate_orm_test_3;
DROP DATABASE IF EXISTS hibernate_orm_test_4;
DROP DATABASE IF EXISTS hibernate_orm_test;
DROP ROLE IF EXISTS hibernate_orm_test;
CREATE ROLE hibernate_orm_test SUPERUSER LOGIN PASSWORD 'hibernate_orm_test';
CREATE DATABASE hibernate_orm_test OWNER hibernate_orm_test;
CREATE DATABASE hibernate_orm_test_1 OWNER hibernate_orm_test;
CREATE DATABASE hibernate_orm_test_2 OWNER hibernate_orm_test;
CREATE DATABASE hibernate_orm_test_3 OWNER hibernate_orm_test;
CREATE DATABASE hibernate_orm_test_4 OWNER hibernate_orm_test;
