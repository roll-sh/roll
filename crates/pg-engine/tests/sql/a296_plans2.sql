-- agent 296, second candidate: XMLTABLE's scan is as wide as the columns read, at their types'
-- widths; a partitioned table's member scans compute the projection over its Append (cost and
-- width); a grouping sets aggregation returns the GROUP BY keys the select list lacks.
create table a296x (data xml) with (autovacuum_enabled = off);
explain select x.* from a296x, xmltable('/rows/row' passing data columns id int path '@id', n for ordinality, name text path 'name', size float path 'size') x;
explain select x.name from a296x, xmltable('/rows/row' passing data columns id int path '@id', name text path 'name') x;
create table a296lp (a int, b text, c numeric) partition by list (a);
create table a296lp1 partition of a296lp for values in (1);
create table a296lp2 partition of a296lp for values in (2);
explain select tableoid::regclass::text, * from a296lp order by 1, 2;
create table a296g (a int, b int, c int) with (autovacuum_enabled = off);
explain select sum(c) from a296g group by grouping sets ((a, b), ()) order by 1 desc;
explain select sum(c) from a296g group by rollup (a, b) order by 1;
