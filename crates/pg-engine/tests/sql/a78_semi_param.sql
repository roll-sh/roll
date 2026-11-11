-- Round 21, agent 78: an EXISTS pulled up as a semi join of one of three inner-joined
-- tables, whose class equates its right side's column with theirs (Hibernate's
-- BulkManipulationTest): an index scan above the semi join takes its parameter from a column
-- the engine's semi join returns.
create table a78_animal (id bigint primary key, description varchar(255), body_weight float4,
  mother_id bigint references a78_animal, father_id bigint references a78_animal, zoo_id bigint,
  serialnumber varchar(255));
create table a78_mammal (animal bigint primary key references a78_animal, pregnant boolean,
  birthdate date, mammalzoo_id bigint, name varchar(255));
create table a78_human (mammal bigint primary key references a78_mammal, name_first varchar(255),
  name_initial char(1), name_last varchar(255), nickname varchar(255),
  height_centimeters float(53) not null, intvalue integer, floatvalue float4,
  bigdecimalvalue numeric(38,2), bigintegervalue numeric(38,0));
create table a78_human_friends (human1 bigint references a78_human,
  human2 bigint not null references a78_human);
insert into a78_animal (id, description) values (1, 'a'), (2, 'b'), (3, 'c');
insert into a78_mammal (animal) values (1), (2), (3);
insert into a78_human (mammal, name_last, height_centimeters)
  values (1, 'Public', 1), (2, 'Doe', 2), (3, 'Public', 3);
insert into a78_human_friends values (1, 3), (2, 1), (3, 1);

select h1_0.mammal from a78_human h1_0 join a78_animal h1_1 on h1_0.mammal = h1_1.id
  join a78_mammal h1_2 on h1_0.mammal = h1_2.animal
  where exists (select f1_0.human2 from a78_human_friends f1_0
    join a78_human f1_1 on f1_1.mammal = f1_0.human2
    where f1_1.name_last = 'Public' and h1_0.mammal = f1_0.human1)
  order by 1;
with id_cte (id) as materialized (select h1_0.mammal from a78_human h1_0
    join a78_animal h1_1 on h1_0.mammal = h1_1.id join a78_mammal h1_2 on h1_0.mammal = h1_2.animal
    where exists (select f1_0.human2 from a78_human_friends f1_0
      join a78_human f1_1 on f1_1.mammal = f1_0.human2
      where f1_1.name_last = 'Public' and h1_0.mammal = f1_0.human1)),
  update_cte (id) as (update a78_animal h1_1 set description = 'updated'
    where h1_1.id in (select id.id from id_cte id) returning id)
select count(*) from id_cte id;
select id, description from a78_animal order by id;
-- The same with the friend's side further away (NOT EXISTS: an anti join).
select h1_0.mammal from a78_human h1_0 join a78_animal h1_1 on h1_0.mammal = h1_1.id
  join a78_mammal h1_2 on h1_0.mammal = h1_2.animal
  where not exists (select 1 from a78_human_friends f1_0
    join a78_human f1_1 on f1_1.mammal = f1_0.human2
    where f1_1.name_last = 'Doe' and h1_0.mammal = f1_0.human1)
  order by 1;

drop table a78_human_friends, a78_human, a78_mammal, a78_animal;
