-- Array searches and containment under the input collation: a nondeterministic one finds
-- elements equal to the searched value (difftest seed 3563 #88).
CREATE COLLATION a104_ci (provider = icu, locale = 'und-u-ks-level2', deterministic = false);
SELECT ARRAY['z'] @> ARRAY['Z' COLLATE a104_ci], ARRAY['z'] <@ ARRAY['Z' COLLATE a104_ci], ARRAY['z'] && ARRAY['Z' COLLATE a104_ci];
SELECT '{"x y",z}' @> array_append('{}', 'Z' COLLATE a104_ci);
SELECT ARRAY['z'] @> ARRAY['Z' COLLATE "C"], ARRAY['z'] @> ARRAY['Z' COLLATE "en-x-icu"];
SELECT array_position(ARRAY['z', 'Z'], 'Z' COLLATE a104_ci), array_position(ARRAY['z', 'Z'], 'Z' COLLATE a104_ci, 2), array_positions(ARRAY['z', NULL, 'Z'], 'z' COLLATE a104_ci);
SELECT array_remove(ARRAY['z', 'a', 'Z'], 'Z' COLLATE a104_ci), array_replace(ARRAY['z', 'a'], 'Z' COLLATE a104_ci, 'q');
SELECT array_position(ARRAY['z', 'Z'], 'Z'), array_remove(ARRAY['z', 'Z'], 'Z' COLLATE "C");
CREATE TABLE a104_t (id int, c text COLLATE a104_ci, cs text[] COLLATE a104_ci);
INSERT INTO a104_t VALUES (1, 'Z', '{z,a}'), (2, 'b', '{B}'), (3, NULL, '{x}');
SELECT id FROM a104_t WHERE cs @> ARRAY['A'] ORDER BY id;
SELECT id FROM a104_t WHERE '{"x y",z}' @> array_append('{}', c) ORDER BY id;
SELECT x.u, t.id FROM unnest('{2,NULL}'::int[]) x(u) RIGHT JOIN a104_t t ON '{"x y",z}' @> array_append('{}', t.c) ORDER BY 2, 1;
SELECT id, array_position(cs, 'Z'), array_remove(cs, 'A') FROM a104_t ORDER BY id;
DROP TABLE a104_t;
DROP COLLATION a104_ci;
