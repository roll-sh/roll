-- cube, earthdistance and seg beyond their contrib tests: binary output, btree indexes,
-- the accessors, and dropping an extension another requires.
CREATE EXTENSION cube;
SELECT cube_send('(1,2),(3,4)'::cube), cube_send('(5)'::cube);
SELECT cube_coord_llur('(1,2),(3,4)'::cube, 3), cube_coord_llur('(1,2),(3,4)'::cube, -4), cube_ll_coord('(5,1),(1,5)', 1);
SELECT cube_coord('(1,2),(3,4)'::cube, 5);
SELECT cube_enlarge('(1,2),(3,4)'::cube, -1.5, 2), cube_enlarge('(0)'::cube, 2, 3);
CREATE TABLE a39_cb (c cube PRIMARY KEY);
INSERT INTO a39_cb VALUES ('(1,2)'), ('(0,5)'), ('(1,2),(3,4)'), ('(2,1)');
INSERT INTO a39_cb VALUES ('(2,1)');
SELECT c FROM a39_cb ORDER BY c;
SELECT c, count(*) FROM (SELECT c FROM a39_cb UNION ALL SELECT '(1,2)') s GROUP BY c ORDER BY c;
SELECT c FROM a39_cb WHERE c = '(1,2)';
SELECT min(c), max(c) FROM a39_cb;
CREATE EXTENSION earthdistance;
SELECT round(('(1,2)'::point <@> '(3,4)'::point)::numeric, 6);
DROP EXTENSION cube;
DROP EXTENSION cube CASCADE;
CREATE EXTENSION seg;
SELECT seg_center('1 .. 4'), seg_size('1 .. 4'), seg_lower('<1 .. >4'), seg_upper('~2 .. ');
SELECT seg_union('1 .. 2', '1.50 .. 3.0'), seg_inter('1 .. 2', '1.50 .. 3.0');
SELECT s FROM unnest(ARRAY['1 .. 2', '1.0 .. 2', '<1 .. 2', '~1 .. 2', '.. 2', '1 ..']::seg[]) s ORDER BY s;
SELECT '1e40'::seg;
SELECT '1 2'::seg;
DROP EXTENSION seg;
