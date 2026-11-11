create TEMP TABLE test_jsonb_subscript (
       id int,
       test_json jsonb
);

insert into test_jsonb_subscript values
(1, '{}'), -- empty jsonb
(2, '{"key": "value"}'); -- jsonb with data

-- update empty jsonb
update test_jsonb_subscript set test_json['a'] = '1' where id = 1;
select * from test_jsonb_subscript;

-- update jsonb with some data
update test_jsonb_subscript set test_json['a'] = '1' where id = 2;
select * from test_jsonb_subscript;

-- replace jsonb
update test_jsonb_subscript set test_json['a'] = '"test"';
select * from test_jsonb_subscript;

-- replace by object
update test_jsonb_subscript set test_json['a'] = '{"b": 1}'::jsonb;
select * from test_jsonb_subscript;

-- replace by array
update test_jsonb_subscript set test_json['a'] = '[1, 2, 3]'::jsonb;
select * from test_jsonb_subscript;

-- use jsonb subscription in where clause
select * from test_jsonb_subscript where test_json['key'] = '"value"';
select * from test_jsonb_subscript where test_json['key_doesnt_exists'] = '"value"';
select * from test_jsonb_subscript where test_json['key'] = '"wrong_value"';

-- NULL
update test_jsonb_subscript set test_json[NULL] = '1';
update test_jsonb_subscript set test_json['another_key'] = NULL;
select * from test_jsonb_subscript;

-- NULL as jsonb source
insert into test_jsonb_subscript values (3, NULL);
update test_jsonb_subscript set test_json['a'] = '1' where id = 3;
select * from test_jsonb_subscript;

update test_jsonb_subscript set test_json = NULL where id = 3;
update test_jsonb_subscript set test_json[0] = '1';
select * from test_jsonb_subscript;

-- Fill the gaps logic
delete from test_jsonb_subscript;
insert into test_jsonb_subscript values (1, '[0]');

update test_jsonb_subscript set test_json[5] = '1';
select * from test_jsonb_subscript;

update test_jsonb_subscript set test_json[-4] = '1';
select * from test_jsonb_subscript;

update test_jsonb_subscript set test_json[-8] = '1';
select * from test_jsonb_subscript;

-- keep consistent values position
delete from test_jsonb_subscript;
insert into test_jsonb_subscript values (1, '[]');

update test_jsonb_subscript set test_json[5] = '1';
select * from test_jsonb_subscript;

-- create the whole path
delete from test_jsonb_subscript;
insert into test_jsonb_subscript values (1, '{}');
update test_jsonb_subscript set test_json['a'][0]['b'][0]['c'] = '1';
select * from test_jsonb_subscript;

delete from test_jsonb_subscript;
insert into test_jsonb_subscript values (1, '{}');
update test_jsonb_subscript set test_json['a'][2]['b'][2]['c'][2] = '1';
select * from test_jsonb_subscript;

-- create the whole path with already existing keys
delete from test_jsonb_subscript;
insert into test_jsonb_subscript values (1, '{"b": 1}');
update test_jsonb_subscript set test_json['a'][0] = '2';
select * from test_jsonb_subscript;

-- the start jsonb is an object, first subscript is treated as a key
delete from test_jsonb_subscript;
insert into test_jsonb_subscript values (1, '{}');
update test_jsonb_subscript set test_json[0]['a'] = '1';
select * from test_jsonb_subscript;

-- the start jsonb is an array
delete from test_jsonb_subscript;
insert into test_jsonb_subscript values (1, '[]');
update test_jsonb_subscript set test_json[0]['a'] = '1';
update test_jsonb_subscript set test_json[2]['b'] = '2';
select * from test_jsonb_subscript;

-- overwriting an existing path
delete from test_jsonb_subscript;
insert into test_jsonb_subscript values (1, '{}');
update test_jsonb_subscript set test_json['a']['b'][1] = '1';
update test_jsonb_subscript set test_json['a']['b'][10] = '1';
select * from test_jsonb_subscript;

delete from test_jsonb_subscript;
insert into test_jsonb_subscript values (1, '[]');
update test_jsonb_subscript set test_json[0][0][0] = '1';
update test_jsonb_subscript set test_json[0][0][1] = '1';
select * from test_jsonb_subscript;

delete from test_jsonb_subscript;
insert into test_jsonb_subscript values (1, '{}');
update test_jsonb_subscript set test_json['a']['b'][10] = '1';
update test_jsonb_subscript set test_json['a'][10][10] = '1';
select * from test_jsonb_subscript;

-- an empty sub element

delete from test_jsonb_subscript;
insert into test_jsonb_subscript values (1, '{"a": {}}');
update test_jsonb_subscript set test_json['a']['b']['c'][2] = '1';
select * from test_jsonb_subscript;

delete from test_jsonb_subscript;
insert into test_jsonb_subscript values (1, '{"a": []}');
update test_jsonb_subscript set test_json['a'][1]['c'][2] = '1';
select * from test_jsonb_subscript;

-- trying replace assuming a composite object, but it's an element or a value

delete from test_jsonb_subscript;
insert into test_jsonb_subscript values (1, '{"a": 1}');
update test_jsonb_subscript set test_json['a']['b'] = '1';
update test_jsonb_subscript set test_json['a']['b']['c'] = '1';
update test_jsonb_subscript set test_json['a'][0] = '1';
update test_jsonb_subscript set test_json['a'][0]['c'] = '1';
update test_jsonb_subscript set test_json['a'][0][0] = '1';

-- trying replace assuming a composite object, but it's a raw scalar

delete from test_jsonb_subscript;
insert into test_jsonb_subscript values (1, 'null');
update test_jsonb_subscript set test_json[0] = '1';
update test_jsonb_subscript set test_json[0][0] = '1';

-- try some things with short-header and toasted subscript values

drop table test_jsonb_subscript;
create temp table test_jsonb_subscript (
       id text,
       test_json jsonb
);

insert into test_jsonb_subscript values('foo', '{"foo": "bar"}');
insert into test_jsonb_subscript
  select s, ('{"' || s || '": "bar"}')::jsonb from repeat('xyzzy', 500) s;
select length(id), test_json[id] from test_jsonb_subscript;
update test_jsonb_subscript set test_json[id] = '"baz"';
select length(id), test_json[id] from test_jsonb_subscript;
update test_jsonb_subscript set test_json[1.5] = '1';
drop table test_jsonb_subscript;
