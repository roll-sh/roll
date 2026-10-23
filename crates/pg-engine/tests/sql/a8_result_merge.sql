-- remove_useless_result_rtes over pulled-up FROM-less relations (join 347's shape): one-row
-- VALUES under outer joins, a left join of two of them on a clause folded to true, and an
-- outer join reduced to inner merging the empty relation into its other side.
create temp table rm_i4 (f1 int);
insert into rm_i4 values (0), (123456), (-123456), (2147483647), (-2147483647);
create temp table rm_tk (unique1 int, unique2 int, stringu1 text, stringu2 text);
insert into rm_tk select g, (g * 7) % 1000, 'a' || g, 'b' || g from generate_series(0, 999) g;
create index rm_tk_u1 on rm_tk (unique1);
create index rm_tk_u2 on rm_tk (unique2);
analyze rm_tk;
analyze rm_i4;

explain (costs off)
select t1.unique2, t1.stringu1, t2.unique1, t2.stringu2 from
  rm_tk t1
  inner join rm_i4 i1
    left join (select v1.x2, v2.y1, 11 AS d1
               from (values(1,0)) v1(x1,x2)
               left join (values(3,1)) v2(y1,y2)
               on v1.x1 = v2.y2) subq1
    on (i1.f1 = subq1.x2)
  on (t1.unique2 = subq1.d1)
  left join rm_tk t2
  on (subq1.y1 = t2.unique1)
where t1.unique2 < 42 and t1.stringu1 < t2.stringu2;

select t1.unique2, t1.stringu1, t2.unique1, t2.stringu2 from
  rm_tk t1
  inner join rm_i4 i1
    left join (select v1.x2, v2.y1, 11 AS d1
               from (values(1,0)) v1(x1,x2)
               left join (values(3,1)) v2(y1,y2)
               on v1.x1 = v2.y2) subq1
    on (i1.f1 = subq1.x2)
  on (t1.unique2 = subq1.d1)
  left join rm_tk t2
  on (subq1.y1 = t2.unique1)
where t1.unique2 < 42 and t1.stringu1 < t2.stringu2;

-- The inner VALUES not matching: its PlaceHolderVars go NULL with it.
select i1.f1, subq1.*
from rm_i4 i1
  left join (select v1.x2, v2.y1, 11 AS d1
             from (values(1,0)) v1(x1,x2)
             left join (values(3,2)) v2(y1,y2)
             on v1.x1 = v2.y2) subq1
  on (i1.f1 = subq1.x2)
order by 1;

-- A one-row VALUES under a left join that stays one.
select i.f1, s.x, s.c from rm_i4 i
  left join (select v.x, 5 as c from (values (0, 1)) v(x, y)) s on s.x = i.f1
order by 1;

-- Reduced to an inner join by a strict WHERE clause over the subquery's column.
select i.f1, s.x, s.c from rm_i4 i
  left join (select v.x, 5 as c from (values (0, 1)) v(x, y)) s on s.x = i.f1
where s.x = 0
order by 1;

-- A full join of an empty relation's values.
select i.f1, s.x, s.c from rm_i4 i
  full join (select v.x, 5 as c from (values (0, 1)) v(x, y)) s on s.x = i.f1
order by 1, 2;
