-- agent 274: a hash join's char key from a join's nullable side keeps its column's
-- statistics (the char(1) to bpchar cast is a RelabelType); a left join reduced to an
-- inner join puts its ON equality into the equivalence class first.
create table a274a (code char primary key);
create table a274b (a char not null, num int not null, primary key (a, num));
create table a274c (name char primary key, a char);
explain select * from a274c left join a274a on a274c.a = a274a.code left join a274b on a274a.code = a274b.a;
explain select * from a274c left join a274a on a274c.a = a274a.code join a274b on a274a.code = a274b.a;
create table a274ai (code int primary key);
create table a274bi (a int not null, num int not null, primary key (a, num));
create table a274ci (name int primary key, a int);
explain select * from a274ci left join a274ai on a274ci.a = a274ai.code join a274bi on a274ai.code = a274bi.a;
