-- PostgREST's database (postgrest_test): an `api` schema exposed to the anonymous role, with
-- the object kinds its schema cache reads (tables, views, a materialized view, foreign keys:
-- one-to-many, many-to-many through a junction, self-referencing, composite; functions:
-- scalar, set-returning, row-returning, volatile, computed columns, variadic, with defaults),
-- comments for its OpenAPI output, and data. A statement the engine lacks fails here alone;
-- the setup logs of both servers show which.
DROP DATABASE IF EXISTS postgrest_test;
DROP ROLE IF EXISTS pgrst_authenticator;
DROP ROLE IF EXISTS pgrst_anon;
CREATE ROLE pgrst_anon NOLOGIN;
CREATE ROLE pgrst_authenticator LOGIN NOINHERIT PASSWORD 'pw';
GRANT pgrst_anon TO pgrst_authenticator;
CREATE DATABASE postgrest_test TEMPLATE template0 ENCODING 'UTF8';
\c postgrest_test
CREATE SCHEMA api;
COMMENT ON SCHEMA api IS 'Bookshop API
The REST interface of the bookshop.';
SET search_path = api, public;
CREATE TYPE mood AS ENUM ('sad', 'ok', 'happy');
CREATE DOMAIN isbn AS text CHECK (VALUE ~ '^[0-9-]{10,17}$');
CREATE TABLE authors (
  id serial PRIMARY KEY,
  name text NOT NULL,
  born date,
  tags text[] NOT NULL DEFAULT '{}',
  mood mood DEFAULT 'ok',
  bio jsonb
);
COMMENT ON TABLE authors IS 'People who write books';
COMMENT ON COLUMN authors.name IS 'Full name';
CREATE TABLE publishers (id int PRIMARY KEY, name text UNIQUE NOT NULL, country char(2));
CREATE TABLE books (
  id serial PRIMARY KEY,
  author_id int NOT NULL REFERENCES authors ON DELETE CASCADE,
  title text NOT NULL,
  isbn isbn UNIQUE,
  pages int CHECK (pages > 0),
  price numeric(8, 2) NOT NULL DEFAULT 0,
  published date,
  rating real,
  meta jsonb NOT NULL DEFAULT '{}',
  search tsvector GENERATED ALWAYS AS (to_tsvector('english', title)) STORED,
  added timestamptz NOT NULL DEFAULT '2025-01-01 00:00:00+00'
);
COMMENT ON COLUMN books.price IS 'In euros';
CREATE INDEX books_author ON books (author_id);
CREATE TABLE book_publishers (
  book_id int REFERENCES books ON DELETE CASCADE,
  publisher_id int REFERENCES publishers,
  edition int NOT NULL DEFAULT 1,
  PRIMARY KEY (book_id, publisher_id)
);
CREATE TABLE reviews (
  book_id int REFERENCES books ON DELETE CASCADE,
  reviewer text,
  stars smallint NOT NULL CHECK (stars BETWEEN 1 AND 5),
  body text,
  period tstzrange,
  PRIMARY KEY (book_id, reviewer)
);
CREATE TABLE review_votes (
  book_id int,
  reviewer text,
  voter text,
  up boolean NOT NULL,
  FOREIGN KEY (book_id, reviewer) REFERENCES reviews ON DELETE CASCADE
);
CREATE TABLE employees (
  id int PRIMARY KEY,
  name text NOT NULL,
  manager_id int REFERENCES employees,
  salary numeric,
  hired date
);
CREATE TABLE events (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY, kind text, at timestamptz, payload json, ip inet, id_uuid uuid);
CREATE VIEW book_summary AS
  SELECT b.id, b.title, a.name AS author, b.price, count(r.*) AS reviews, avg(r.stars)::numeric(3, 2) AS stars
  FROM books b JOIN authors a ON a.id = b.author_id LEFT JOIN reviews r ON r.book_id = b.id
  GROUP BY b.id, a.name;
COMMENT ON VIEW book_summary IS 'One row per book';
CREATE VIEW cheap_books AS SELECT * FROM books WHERE price < 20;
CREATE MATERIALIZED VIEW author_stats AS
  SELECT author_id, count(*) AS books, sum(pages) AS pages FROM books GROUP BY author_id;
CREATE FUNCTION add_them(a int, b int DEFAULT 10) RETURNS int LANGUAGE sql IMMUTABLE AS 'SELECT a + b';
CREATE FUNCTION search_books(q text) RETURNS SETOF books LANGUAGE sql STABLE
  AS $$ SELECT * FROM api.books WHERE search @@ plainto_tsquery('english', q) ORDER BY id $$;
CREATE FUNCTION get_author(author_id int) RETURNS authors LANGUAGE sql STABLE
  AS 'SELECT * FROM api.authors WHERE id = author_id';
CREATE FUNCTION books_full_title(books) RETURNS text LANGUAGE sql STABLE
  AS $$ SELECT $1.title || ' (' || $1.pages || ' pages)' $$;
CREATE FUNCTION authors_book_count(authors) RETURNS bigint LANGUAGE sql STABLE
  AS 'SELECT count(*) FROM api.books WHERE author_id = $1.id';
CREATE FUNCTION bump_price(book int, pct numeric) RETURNS numeric LANGUAGE plpgsql VOLATILE AS $$
DECLARE p numeric;
BEGIN
  UPDATE api.books SET price = round(price * (1 + pct / 100), 2) WHERE id = book RETURNING price INTO p;
  IF p IS NULL THEN RAISE EXCEPTION 'no book %', book USING ERRCODE = 'P0002', HINT = 'check the id'; END IF;
  RETURN p;
END $$;
CREATE FUNCTION sum_all(VARIADIC xs int[]) RETURNS int LANGUAGE sql IMMUTABLE AS 'SELECT sum(x)::int FROM unnest(xs) x';
CREATE FUNCTION echo_json(j jsonb) RETURNS jsonb LANGUAGE sql IMMUTABLE AS 'SELECT j';
CREATE FUNCTION top_rated(min_stars int DEFAULT 4) RETURNS TABLE (title text, stars numeric) LANGUAGE sql STABLE
  AS $$ SELECT title, stars FROM api.book_summary WHERE stars >= min_stars ORDER BY stars DESC, title $$;
COMMENT ON FUNCTION add_them(int, int) IS 'Adds two numbers';
INSERT INTO authors (name, born, tags, mood, bio) VALUES
  ('Ursula Le Guin', '1929-10-21', '{scifi,fantasy}', 'happy', '{"country": "US", "awards": 21}'),
  ('Stanislaw Lem', '1921-09-12', '{scifi}', 'ok', '{"country": "PL"}'),
  ('Italo Calvino', '1923-10-15', '{literary}', 'happy', NULL),
  ('Anonymous', NULL, '{}', NULL, '{"country": null}');
INSERT INTO publishers VALUES (1, 'Ace', 'US'), (2, 'Harcourt', 'US'), (3, 'Einaudi', 'IT');
INSERT INTO books (author_id, title, isbn, pages, price, published, rating, meta) VALUES
  (1, 'The Left Hand of Darkness', '978-0441478125', 304, 9.99, '1969-03-01', 4.5, '{"series": "Hainish", "awards": ["Hugo", "Nebula"]}'),
  (1, 'The Dispossessed', '978-0061054884', 387, 15.50, '1974-05-01', 4.6, '{"series": "Hainish"}'),
  (1, 'A Wizard of Earthsea', '978-0547773742', 183, 8.25, '1968-01-01', 4.3, '{"series": "Earthsea"}'),
  (2, 'Solaris', '978-0156027601', 204, 12.00, '1961-01-01', 4.2, '{}'),
  (2, 'The Cyberiad', '978-0156027595', 295, 22.40, '1965-01-01', 4.4, '{"translator": "Kandel"}'),
  (3, 'Invisible Cities', '978-0156453806', 165, 11.75, '1972-01-01', 4.1, '{"translator": "Weaver"}'),
  (3, 'If on a winter''s night a traveler', '978-0156439619', 260, 25.00, '1979-01-01', NULL, '{}');
INSERT INTO book_publishers VALUES (1, 1, 1), (2, 2, 3), (3, 2, 1), (4, 2, 1), (5, 2, 2), (6, 3, 1), (6, 2, 2), (7, 2, 1);
INSERT INTO reviews VALUES
  (1, 'ann', 5, 'Superb', '[2024-01-01,2024-02-01)'), (1, 'bob', 4, 'Good', NULL),
  (2, 'ann', 5, NULL, '[2024-03-01,)'), (4, 'cy', 3, 'Strange', NULL), (6, 'bob', 4, 'Dreamy', NULL),
  (6, 'cy', 2, 'Lost me', '(,2023-01-01)');
INSERT INTO review_votes VALUES (1, 'ann', 'bob', true), (1, 'ann', 'cy', true), (6, 'cy', 'ann', false);
INSERT INTO employees VALUES (1, 'Boss', NULL, 9000, '2010-01-01'), (2, 'Mid', 1, 5000, '2015-06-01'),
  (3, 'Junior', 2, 3000, '2022-09-15'), (4, 'Other', 1, 5100, '2016-02-29');
INSERT INTO events (kind, at, payload, ip, id_uuid) VALUES
  ('login', '2025-03-01 10:00:00+00', '{"user": "ann"}', '10.0.0.1', '6ba7b810-9dad-11d1-80b4-00c04fd430c8'),
  ('logout', '2025-03-01 11:30:00+00', '{"user": "ann"}', '10.0.0.1', NULL),
  ('login', '2025-03-02 09:15:00+00', '{"user": "bob", "via": ["web"]}', '192.168.1.20', NULL);
REFRESH MATERIALIZED VIEW author_stats;
GRANT USAGE ON SCHEMA api TO pgrst_anon;
GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA api TO pgrst_anon;
GRANT USAGE ON ALL SEQUENCES IN SCHEMA api TO pgrst_anon;
GRANT EXECUTE ON ALL FUNCTIONS IN SCHEMA api TO pgrst_anon;
REVOKE INSERT, UPDATE, DELETE ON employees FROM pgrst_anon;
GRANT UPDATE (salary) ON employees TO pgrst_anon;
