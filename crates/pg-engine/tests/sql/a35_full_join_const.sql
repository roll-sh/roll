-- Found by the differential tester (tests/difftest.rs). A boolean test of a constant
-- (`NULL IS FALSE`) is folded, so a FULL JOIN on it is a FULL JOIN ON false, which needs
-- no merge or hash join.
CREATE TABLE ta (a int);
CREATE TABLE tb (b int);
INSERT INTO ta VALUES (1), (2);
INSERT INTO tb VALUES (3);
SELECT * FROM ta FULL JOIN tb ON false;
SELECT * FROM ta FULL JOIN tb ON NULL IS FALSE;
SELECT * FROM ta FULL JOIN tb ON NULL IS NOT TRUE AND a = b;
SELECT * FROM ta FULL JOIN tb ON true IS UNKNOWN;
SELECT * FROM ta FULL JOIN tb ON a = b AND false;
SELECT * FROM ta FULL JOIN tb ON a < b AND false IS TRUE;
SELECT * FROM ta FULL JOIN tb ON a < b;
SELECT * FROM (VALUES (1), (2)) s1 (u) FULL JOIN (VALUES (3)) t2 ON NULL;
SELECT NULL::bool IS TRUE, NULL::bool IS NOT TRUE, false IS FALSE, true IS NOT FALSE, NULL::bool IS UNKNOWN, false IS NOT UNKNOWN;
SELECT a FROM ta WHERE NULL::bool IS NOT FALSE;
EXPLAIN (COSTS OFF) SELECT a FROM ta WHERE NULL::bool IS UNKNOWN;
