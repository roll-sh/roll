-- Laravel's CI PostgreSQL service: POSTGRES_USER forge / password (a superuser), database
-- laravel.
DROP DATABASE IF EXISTS laravel;
DROP ROLE IF EXISTS forge;
CREATE ROLE forge SUPERUSER LOGIN PASSWORD 'password';
CREATE DATABASE laravel OWNER forge;
