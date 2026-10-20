-- Row estimates with extended statistics: n-distinct coefficients for GROUP BY, functional
-- dependencies and MCV lists for restrictions, expression statistics; plus scalar-array
-- estimates (= ANY of an array column, NULL elements, NOT IN).
create function est(q text) returns int language plpgsql as $$
declare
  ln text;
begin
  for ln in execute 'explain ' || q loop
    return substring(ln from ' rows=(\d+)')::int;
  end loop;
end $$;
create table ge (a int, b int, c int, d varchar, ia int[]) with (autovacuum_enabled = off);
insert into ge select i % 50, i % 50 / 5, i % 7, (i % 25)::text, array[i % 10, i % 3]
  from generate_series(1, 5000) i;
analyze ge;
select est('select * from ge where a = 1 and b = 0');
select est('select * from ge where a = 1 and d = ''1''');
select est('select a, b from ge group by a, b');
select est('select a from ge group by a, (b + 1)');
select est('select 1 from ge group by (a + 1), (c * 2)');
select est('select 1 from ge group by a > 3, b');
select est('select * from ge where a in (1, 2) and b = 0');
select est('select * from ge where a not in (1, 2)');
select est('select * from ge where 4 = any(ia)');
select est('select * from ge where 11 = any(ia)');
select est('select * from ge where (a + 1) = any(array[1, null, 3])');
create statistics ge_s1 (ndistinct, dependencies) on a, b from ge;
create statistics ge_s2 (mcv) on a, c, d from ge;
create statistics ge_s3 on (a + 1), (c * 2) from ge;
analyze ge;
select est('select * from ge where a = 1 and b = 0');
select est('select * from ge where a = 1 and b in (0, 1)');
select est('select * from ge where (a = 1 or a = 2) and b = 0');
select est('select * from ge where a = 1 and d = ''1''');
select est('select * from ge where a = 1 and c = 1 and d = ''1''');
select est('select * from ge where a < 5 and c < 3');
select est('select * from ge where a = 1 or c = 1');
select est('select * from ge where (a = 1 and c = 1) or (a = 2 and c = 2)');
select est('select * from ge where a is null and c = 1');
select est('select * from ge where not (a = 1) and c = 1');
select est('select a, b from ge group by a, b');
select est('select a, b from ge group by b, a');
select est('select 1 from ge group by (a + 1), (c * 2)');
select est('select 1 from ge group by (a + 1)');
select est('select * from ge where (a + 1) = 2');
select est('select * from ge where (a + 1) = 2 and (c * 2) = 2');
drop table ge;
drop function est(text);
