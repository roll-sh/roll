CREATE TABLE users (id uuid PRIMARY KEY, email text, created timestamp);
CREATE INDEX users_email ON users (email);
