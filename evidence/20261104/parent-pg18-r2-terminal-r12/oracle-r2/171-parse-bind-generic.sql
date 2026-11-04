-- CACHE-ONLY / NOT RUN. Parse ONCE at seq_page_cost500; six binds after RESET.
CREATE TABLE p171_generic(a int,b text);
INSERT INTO p171_generic SELECT g,g::text FROM generate_series(1,100) g;
ANALYZE p171_generic;
SET seq_page_cost=500;
SELECT b FROM p171_generic WHERE a=$1 \parse p171_generic_plan
RESET seq_page_cost;
\bind_named p171_generic_plan 1
\g
\bind_named p171_generic_plan 1
\g
\bind_named p171_generic_plan 1
\g
\bind_named p171_generic_plan 1
\g
\bind_named p171_generic_plan 1
\g
\bind_named p171_generic_plan 1
\g
SELECT generic_plans,custom_plans FROM pg_prepared_statements WHERE name='p171_generic_plan';
