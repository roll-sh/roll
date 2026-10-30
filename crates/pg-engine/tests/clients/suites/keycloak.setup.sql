-- Keycloak's database (keycloak_app): its Liquibase changelog makes the tables at startup.
DROP DATABASE IF EXISTS keycloak_app;
CREATE DATABASE keycloak_app;
