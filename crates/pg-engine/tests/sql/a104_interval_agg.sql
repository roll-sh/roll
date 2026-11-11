-- sum / avg of intervals: the finite values add up apart from the infinities (an overflow
-- there fails even next to an infinity), infinities of both signs fail, avg divides
-- (difftest seed 5007 #46).
SELECT sum(v), avg(v) FROM (VALUES (interval '-infinity'), ('-178000000 years'), ('-178000000 years')) t(v);
SELECT sum(v), avg(v) FROM (VALUES (interval '-infinity'), ('-1 year'), ('infinity')) t(v);
SELECT sum(v), avg(v) FROM (VALUES (interval 'infinity'), ('1 year'), (NULL)) t(v);
SELECT sum(v), avg(v) FROM (VALUES (interval '-infinity'), ('1 year')) t(v);
SELECT sum(v), avg(v) FROM (VALUES (interval '1 day 1 sec'), ('2 mons'), ('7 days 00:00:00.000001')) t(v);
SELECT avg(v) FROM (VALUES (interval '1 mon'), ('1 mon'), ('1 day')) t(v);
SELECT avg(v) FROM (VALUES (interval '1 sec'), ('0'), ('0')) t(v);
SELECT sum(v), avg(v) FROM (VALUES (NULL::interval)) t(v);
SELECT sum(v) FROM (VALUES (interval '2147483647 days'), ('1 day')) t(v);
SELECT k, sum(v) OVER (ORDER BY k), avg(v) OVER (ORDER BY k ROWS 1 PRECEDING) FROM (VALUES (1, interval '1 day'), (2, 'infinity'), (3, '2 days'), (4, '-infinity')) t(k, v);
