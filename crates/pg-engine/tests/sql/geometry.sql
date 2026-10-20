-- Geometric types: point, line, lseg, box, path, polygon, circle.
SELECT '(1,2)'::point, ' ( 1.5 , -2e3 ) '::point, '1,2'::point, '(inf,nan)'::point, '(-0,0)'::point, point(3, 4);
SELECT '(1,2'::point;
SELECT 'x'::point;
SELECT '(1,2,3)'::point;
SELECT '(1e400,0)'::point;
SELECT '{1,2,3}'::line, '[(0,0),(1,1)]'::line, '(0,0),(0,1)'::line, '((1,1),(2,1))'::line, line(point(0,0), point(2,1));
SELECT '{0,0,1}'::line;
SELECT '[(1,1),(1,1)]'::line;
SELECT '[(1,2),(3,4)]'::lseg, '(1,2),(3,4)'::lseg, '((1,2),(3,4))'::lseg, '1,2,3,4'::lseg, lseg(point(0,0), point(1,1));
SELECT '(1,1),(0,0)'::box, '(0,0),(1,1)'::box, '((3,1),(1,3))'::box, '1,2,3,4'::box, box(point(2,2), point(0,0)), box(point(1,1));
SELECT '[(1,1),(1,1)]'::box;
SELECT '[(0,0),(1,1),(2,0)]'::path, '((0,0),(1,1),(2,0))'::path, '(0,0),(1,1),(2,0)'::path, '(1,2)'::path, '((1,2))'::path, '[1,2,3,4]'::path;
SELECT '[(0,0),(1,1)'::path;
SELECT '(0,0),(1,1)]'::path;
SELECT '((0,0),(1,1),(2,0))'::polygon, '(0,0),(1,1),(2,0)'::polygon, '0,0,1,1,2,0'::polygon, '(1,2)'::polygon;
SELECT '[(0,0),(1,1)]'::polygon;
SELECT '<(1,2),3>'::circle, '((1,2),3)'::circle, '(1,2),3'::circle, '1,2,3'::circle, '<(1,2),0>'::circle, circle(point(0,0), 2);
SELECT '<(1,2),-3>'::circle;
SELECT '<(1,2),3'::circle;
-- point operators
SELECT p1, p2, p1 + p2 AS add, p1 - p2 AS sub, p1 * p2 AS mul, p1 / p2 AS div, p1 <-> p2 AS dist FROM (VALUES (point '(1,2)', point '(3,4)'), ('(0,0)', '(1,0)'), ('(-1,5.5)', '(2,-3)')) v(p1, p2);
SELECT point '(1,2)' / point '(0,0)';
SELECT p1 << p2, p1 >> p2, p1 <<| p2, p1 |>> p2, p1 <^ p2, p1 >^ p2, p1 ?- p2, p1 ?| p2, p1 ~= p2, p1 <> p2 FROM (VALUES (point '(1,2)', point '(3,4)'), ('(1,1)', '(1,1.0000001)'), ('(5,1)', '(1,1)')) v(p1, p2);
SELECT slope(point '(0,0)', point '(2,1)'), slope(point '(1,1)', point '(1,5)'), slope(point '(1,1)', point '(5,1)'), isvertical(point '(1,1)', point '(1,5)'), ishorizontal(point '(1,1)', point '(5,1)');
SELECT (point '(1,2)')[0], (point '(1,2)')[1], (point '(1,2)')[2], (point '(1,2)')[-1], (lseg '[(1,2),(3,4)]')[1], (box '(1,2),(3,4)')[0], (line '{1,2,3}')[2], (box '(1,1),(0,0)')[0][1];
SELECT (point '(1,2)')[0:1];
SELECT (circle '<(1,2),3>')[0];
-- point vs others
SELECT point '(1,1)' <@ box '(0,0),(2,2)', point '(3,3)' <@ box '(0,0),(2,2)', point '(0.5,0.5)' <@ lseg '[(0,0),(1,1)]', point '(1,1)' <@ line '{1,-1,0}', point '(1,1)' <@ circle '<(0,0),2>', point '(5,5)' <@ circle '<(0,0),2>';
SELECT point '(1,1)' <@ polygon '((0,0),(2,0),(2,2),(0,2))', point '(2,1)' <@ polygon '((0,0),(2,0),(2,2),(0,2))', point '(3,1)' <@ polygon '((0,0),(2,0),(2,2),(0,2))', point '(1,0)' <@ path '[(0,0),(2,0)]', point '(1,1)' <@ path '((0,0),(2,0),(2,2),(0,2))';
SELECT point '(3,4)' <-> box '(0,0),(1,1)', point '(3,4)' <-> lseg '[(0,0),(0,10)]', point '(3,4)' <-> line '{1,0,0}', point '(3,4)' <-> circle '<(0,0),1>', point '(0,0)' <-> circle '<(0,0),1>', point '(5,5)' <-> path '[(0,0),(1,1),(2,0)]', point '(5,5)' <-> polygon '((0,0),(1,1),(2,0))';
SELECT point '(3,4)' ## box '(0,0),(1,1)', point '(3,4)' ## lseg '[(0,0),(0,10)]', point '(3,4)' ## line '{1,-1,0}', point '(0.5,0.5)' ## box '(0,0),(1,1)';
-- lines
SELECT l1, l2, l1 # l2 AS inter, l1 <-> l2 AS dist, l1 = l2 AS eq, l1 ?# l2 AS isect, l1 ?-| l2 AS perp, l1 ?|| l2 AS par FROM (VALUES (line '{1,-1,0}', line '{1,1,0}'), ('{1,-1,0}', '{2,-2,4}'), ('{0,-1,3}', '{-1,0,2}'), ('{1,2,3}', '{2,4,6}')) v(l1, l2);
SELECT ?- line '{0,1,2}', ?| line '{1,0,2}', ishorizontal(line '{0,1,2}'), isvertical(line '{0,1,2}'), isparallel(line '{1,1,0}', line '{2,2,5}'), isperp(line '{1,-1,0}', line '{1,1,0}');
SELECT line '{1,-1,0}' ?# box '(2,3),(3,2)', line '{1,-1,0}' ?# box '(5,1),(6,0)', line '{1,-1,0}' <-> point '(1,0)', line '{1,-1,0}' <-> lseg '[(2,0),(3,0)]', line '{1,-1,0}' ## lseg '[(2,0),(3,0)]';
-- lseg
SELECT s1 # s2, s1 ## s2, s1 <-> s2, s1 ?# s2, s1 ?-| s2, s1 ?|| s2, s1 = s2, s1 <> s2, s1 < s2, s1 >= s2 FROM (VALUES (lseg '[(0,0),(2,2)]', lseg '[(0,2),(2,0)]'), ('[(0,0),(1,0)]', '[(0,1),(1,1)]'), ('[(0,0),(1,1)]', '[(5,5),(6,8)]'), ('[(0,0),(1,0)]', '[(0,0),(1,0)]')) v(s1, s2);
SELECT @-@ lseg '[(0,0),(3,4)]', @@ lseg '[(0,0),(3,4)]', ?- lseg '[(0,1),(3,1)]', ?| lseg '[(1,1),(1,4)]', length(lseg '[(0,0),(3,4)]'), point(lseg '[(0,0),(3,4)]'), isvertical(lseg '[(1,1),(1,4)]'), isparallel(lseg '[(0,0),(1,1)]', lseg '[(1,0),(2,1)]'), isperp(lseg '[(0,0),(1,1)]', lseg '[(0,1),(1,0)]');
SELECT lseg '[(0,0),(1,1)]' <@ box '(0,0),(2,2)', lseg '[(0,0),(3,3)]' <@ box '(0,0),(2,2)', lseg '[(0,0),(3,3)]' ?# box '(1,0),(2,0.5)', lseg '[(0,0),(3,3)]' ?# box '(1,1.5),(2,2.5)', lseg '[(5,5),(6,6)]' <-> box '(0,0),(1,1)', lseg '[(5,5),(6,6)]' ## box '(0,0),(1,1)';
SELECT lseg '[(0,0),(2,2)]' <-> line '{1,1,-10}', lseg '[(0,0),(2,2)]' <@ line '{1,-1,0}', lseg '[(0,0),(2,2)]' ?# line '{1,1,-2}', lseg '[(0,0),(10,0)]' <-> point '(5,3)';
-- boxes
SELECT b1, b2, b1 && b2, b1 &< b2, b1 &> b2, b1 << b2, b1 >> b2, b1 <@ b2, b1 @> b2, b1 ~= b2, b1 = b2, b1 < b2, b1 # b2, b1 <-> b2 FROM (VALUES (box '(2,2),(0,0)', box '(3,3),(1,1)'), ('(1,1),(0,0)', '(3,3),(2,2)'), ('(3,3),(0,0)', '(2,2),(1,1)'), ('(1,1),(0,0)', '(2,2),(1,1)')) v(b1, b2);
SELECT b1 <<| b2, b1 |>> b2, b1 &<| b2, b1 |&> b2, b1 <^ b2, b1 >^ b2, b1 ?# b2 FROM (VALUES (box '(1,1),(0,0)', box '(1,3),(0,2)'), ('(1,3),(0,2)', '(1,1),(0,0)'), ('(1,2),(0,1)', '(1,1),(0,0)')) v(b1, b2);
SELECT area(box '(2,3),(0,0)'), width(box '(2,3),(0,0)'), height(box '(2,3),(0,0)'), center(box '(2,3),(0,0)'), @@ box '(2,3),(0,0)', diagonal(box '(2,3),(0,0)'), lseg(box '(2,3),(0,0)'), point(box '(2,3),(0,0)'), bound_box(box '(1,1),(0,0)', box '(3,3),(2,2)');
SELECT box '(2,2),(0,0)' + point '(1,1)', box '(2,2),(0,0)' - point '(1,1)', box '(2,2),(0,0)' * point '(2,0)', box '(2,2),(1,1)' / point '(2,0)', box '(2,2),(0,0)' @> point '(1,1)', box '(2,2),(0,0)' <-> point '(3,3)';
SELECT box '(2,2),(0,0)' / point '(0,0)';
SELECT polygon(box '(2,2),(0,0)'), circle(box '(2,2),(0,0)'), box(circle '<(0,0),2>'), box(polygon '((0,0),(3,1),(1,4))');
-- paths
SELECT p, npoints(p), # p, isclosed(p), isopen(p), length(p), @-@ p, area(p), pclose(p), popen(p) FROM (VALUES (path '[(0,0),(3,4),(3,0)]'), ('((0,0),(3,4),(3,0))'), ('[(1,1)]')) v(p);
SELECT path '[(0,0),(1,1)]' + path '[(2,2),(3,3)]', path '((0,0),(1,1))' + path '[(2,2)]', path '[(0,0),(1,1)]' + point '(1,1)', path '[(0,0),(1,1)]' - point '(1,1)', path '[(1,0),(1,1)]' * point '(0,1)', path '[(2,0),(1,1)]' / point '(0,1)';
SELECT path '[(0,0),(2,2)]' ?# path '[(0,2),(2,0)]', path '[(0,0),(1,1)]' ?# path '[(5,5),(6,6)]', path '[(0,0),(1,1)]' <-> path '[(5,5),(6,6)]', path '[(0,0),(1,1)]' = path '[(5,5),(6,6)]', path '[(0,0)]' < path '[(5,5),(6,6)]', path '((0,0),(2,0),(2,2))' @> point '(1.5,0.5)', path '[(0,0),(2,0),(2,2)]' @> point '(1.5,0.5)';
SELECT polygon(path '((0,0),(1,1),(2,0))'), path(polygon '((0,0),(1,1),(2,0))');
SELECT polygon(path '[(0,0),(1,1),(2,0)]');
-- polygons
SELECT p1 && p2, p1 @> p2, p1 <@ p2, p1 ~= p2, p1 << p2, p1 >> p2, p1 &< p2, p1 &> p2, p1 <<| p2, p1 |>> p2, p1 &<| p2, p1 |&> p2, p1 <-> p2 FROM (VALUES (polygon '((0,0),(4,0),(4,4),(0,4))', polygon '((1,1),(2,1),(2,2),(1,2))'), ('((0,0),(1,0),(1,1))', '((2,2),(3,2),(3,3))'), ('((0,0),(2,0),(2,2),(0,2))', '((0,2),(2,2),(2,0),(0,0))'), ('((0,0),(2,0),(0,2))', '((1,1),(3,1),(3,3))')) v(p1, p2);
SELECT npoints(polygon '((0,0),(1,1),(2,0))'), # polygon '((0,0),(1,1),(2,0))', @@ polygon '((0,0),(2,0),(2,2),(0,2))', center(circle(polygon '((0,0),(2,0),(2,2),(0,2))')), point(polygon '((0,0),(2,0),(2,2),(0,2))'), box(polygon '((0,0),(1,1),(2,0))'), polygon '((0,0),(2,0),(2,2),(0,2))' @> point '(1,1)', polygon '((0,0),(2,0),(2,2),(0,2))' <-> point '(5,1)', polygon '((0,0),(2,0),(2,2),(0,2))' <-> circle '<(5,1),1>';
SELECT polygon(4, circle '<(0,0),1>'), npoints(polygon(circle '<(1,1),2>')), polygon(circle '<(1,1),2>') ~= polygon(12, circle '<(1,1),2>') AS same;
SELECT polygon(1, circle '<(0,0),1>');
SELECT polygon(4, circle '<(0,0),0>');
-- circles
SELECT c1, c2, c1 && c2, c1 &< c2, c1 &> c2, c1 << c2, c1 >> c2, c1 <<| c2, c1 |>> c2, c1 &<| c2, c1 |&> c2, c1 <@ c2, c1 @> c2, c1 ~= c2, c1 = c2, c1 <> c2, c1 < c2, c1 >= c2, c1 <-> c2 FROM (VALUES (circle '<(0,0),1>', circle '<(1,1),1>'), ('<(0,0),1>', '<(5,5),1>'), ('<(0,0),5>', '<(1,1),1>'), ('<(0,0),1>', '<(0,0),1>')) v(c1, c2);
SELECT area(circle '<(0,0),2>'), radius(circle '<(0,0),2>'), diameter(circle '<(0,0),2>'), center(circle '<(1,2),2>'), @@ circle '<(1,2),2>', point(circle '<(1,2),2>'), circle(point '(1,1)', 3), circle '<(1,1),1>' + point '(1,1)', circle '<(1,1),1>' - point '(1,1)', circle '<(1,1),1>' * point '(0,2)', circle '<(1,1),2>' / point '(0,2)';
SELECT circle '<(0,0),2>' @> point '(1,1)', circle '<(0,0),2>' <-> point '(5,0)', circle '<(0,0),1>' <-> polygon '((3,0),(4,0),(4,1))';
-- casts
SELECT point '(1,1)'::box, lseg '[(0,0),(2,2)]'::point, path '((0,0),(1,1),(2,0))'::polygon, box '(2,2),(0,0)'::point, box '(2,2),(0,0)'::lseg, box '(2,2),(0,0)'::polygon, box '(2,2),(0,0)'::circle;
SELECT polygon '((0,0),(2,0),(2,2),(0,2))'::point, polygon '((0,0),(2,0),(2,2),(0,2))'::path, polygon '((0,0),(2,0),(2,2),(0,2))'::box, polygon '((0,0),(2,0),(2,2),(0,2))'::circle, circle '<(1,1),1>'::point, circle '<(0,0),1>'::box;
SELECT npoints(circle '<(0,0),1>'::polygon), circle '<(0,0),1>'::polygon ~= polygon(circle '<(0,0),1>');
SELECT point '(1,1)'::circle;
SELECT point '(1,1)'::text, '(1,1)'::text::point, box '(1,1),(0,0)'::text::box;
-- tables, arrays and storage
CREATE TABLE geo (id int, p point, l line, s lseg, b box, pa path, po polygon, c circle);
INSERT INTO geo VALUES (1, '(1,2)', '{1,2,3}', '[(0,0),(1,1)]', '(1,1),(0,0)', '[(0,0),(1,1)]', '((0,0),(1,1),(1,0))', '<(1,1),2>'), (2, '(-1,-2.5)', '{0,1,0}', '[(1,1),(2,2)]', '(5,5),(3,3)', '((0,0),(1,1),(2,0))', '((0,0),(2,0),(2,2))', '<(0,0),0.5>'), (3, NULL, NULL, NULL, NULL, NULL, NULL, NULL);
SELECT * FROM geo ORDER BY id;
SELECT id FROM geo WHERE p <@ box '(2,3),(0,0)';
SELECT id, b FROM geo WHERE b && box '(4,4),(2,2)';
UPDATE geo SET p[0] = 10, b[1] = point '(-1,-1)' WHERE id = 1;
SELECT p, b FROM geo WHERE id = 1;
UPDATE geo SET p[2] = 10 WHERE id = 1;
UPDATE geo SET p[0] = NULL WHERE id = 1;
UPDATE geo SET p[0] = 1 WHERE id = 3;
SELECT p FROM geo WHERE id = 3;
SELECT ARRAY[point '(1,2)', point '(3,4)'], '{"(1,2)","(3,4)"}'::point[], ARRAY[box '(1,1),(0,0)', box '(2,2),(1,1)'], '{(1,1),(0,0);(2,2),(1,1)}'::box[], ARRAY[circle '<(1,1),1>'];
SELECT p FROM geo ORDER BY p;
SELECT b FROM geo ORDER BY b;
SELECT pg_typeof(p), pg_typeof(l), pg_typeof(s), pg_typeof(b), pg_typeof(pa), pg_typeof(po), pg_typeof(c) FROM geo WHERE id = 1;
CREATE INDEX geo_b ON geo USING gist (b);
CREATE INDEX geo_p ON geo USING gist (p);
CREATE INDEX geo_c ON geo USING gist (c);
CREATE INDEX geo_po ON geo USING gist (po);
SELECT id FROM geo WHERE b && box '(4,4),(2,2)';
CREATE INDEX geo_bad ON geo (p);
SELECT typname, typlen, typcategory, typdelim, typelem::regtype, typarray::regtype FROM pg_type WHERE typname IN ('point', 'line', 'lseg', 'box', 'path', 'polygon', 'circle') ORDER BY oid;
DROP TABLE geo;
