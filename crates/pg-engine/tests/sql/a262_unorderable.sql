-- Types without a default btree ordering (xid, cid, aclitem, and arrays of them): ORDER BY
-- and an aggregate's DISTINCT need one (get_sort_group_operators); hashing doesn't.
create table uo (a int);
insert into uo values (1), (2);
select count(distinct xmax) from uo;
select array_agg(distinct xmin) from uo;
select count(distinct cmin) from uo;
select xmax from uo order by xmax;
select rank() over (order by xmin) from uo;
select percentile_disc(0.5) within group (order by xmax) from uo;
select a from (values (array['1'::xid])) v(a) order by a;
select a from (values ('=r/postgres'::aclitem)) v(a) order by a;
select count(distinct a) from (values ('=r/postgres'::aclitem)) v(a);
select count(distinct a) from (values ('{}'::json)) v(a);
select distinct xmax from uo;
select xmax, count(*) from uo group by xmax;
select a from (values ('1'::xid8), ('2')) v(a) order by a desc;
drop table uo;
