-- A LIMIT over a grouping (no ORDER BY): the grouping path is the cheapest for the
-- fraction of its groups the LIMIT reads (get_cheapest_fractional_path), so a grouping over
-- an index in the keys' order beats a hashed one, and its groups come in key order.
CREATE TABLE lg_orders (id int PRIMARY KEY, customer text NOT NULL, amount numeric)
  WITH (autovacuum_enabled = off);
CREATE INDEX lg_orders_customer ON lg_orders (customer);
INSERT INTO lg_orders SELECT i, 'C' || lpad(((i * 7) % 89)::text, 3, '0'), i % 13 FROM generate_series(1, 830) i;
-- Before any ANALYZE (200 groups estimated).
EXPLAIN SELECT customer, count(*) FROM lg_orders GROUP BY customer LIMIT 5;
SELECT customer, count(*) FROM lg_orders GROUP BY customer LIMIT 5;
EXPLAIN SELECT customer, count(*) FROM lg_orders GROUP BY customer;
ANALYZE lg_orders;
EXPLAIN SELECT customer, count(*) FROM lg_orders GROUP BY customer LIMIT 5;
SELECT customer, count(*) FROM lg_orders GROUP BY customer LIMIT 5;
EXPLAIN SELECT customer, max(amount) FROM lg_orders GROUP BY customer LIMIT 3 OFFSET 2;
SELECT customer, max(amount) FROM lg_orders GROUP BY customer LIMIT 3 OFFSET 2;
-- Most of the groups: hashing stays cheaper.
EXPLAIN SELECT customer, count(*) FROM lg_orders GROUP BY customer LIMIT 80;
-- DISTINCT under a LIMIT.
EXPLAIN SELECT DISTINCT customer FROM lg_orders LIMIT 4;
SELECT DISTINCT customer FROM lg_orders LIMIT 4;
DROP TABLE lg_orders;
