-- Operator implementation functions called by name.
SELECT int4pl(1, 2), int4mi(5, 3), int4mul(4, 5), int4div(9, 2), int4mod(9, 4), int4um(5), int2pl(1::int2, 2::int2);
SELECT int48pl(1, 2::int8), int84mi(5::int8, 3), int8mul(4, 5), pg_typeof(int48pl(1, 2));
SELECT float8mi(3, 1.5), float8pl(1, 2), float8mul(2, 3), float8div(1, 4), float4pl(1.5::float4, 2.5::float4), float48mul(2::float4, 3);
SELECT numeric_add(1.5, 2), numeric_sub(1, 0.25), numeric_mul(2, 3.5), numeric_div(1, 3), numeric_mod(10, 3), numeric_uminus(4.5);
SELECT texteq('a', 'a'), textne('a', 'b'), text_lt('a', 'b'), text_ge('a', 'b'), textcat('ab', 'cd'), int4eq(1, 1), int4lt(1, 2), int8ge(2, 3);
SELECT booleq(true, false), boolne(true, false), date_mi('2020-01-05', '2020-01-01'), date_pli('2020-01-01', 3), date_mii('2020-01-10', 3);
SELECT interval_pl('1 day', '2 hours'), timestamp_mi('2020-01-02', '2020-01-01'), float8eq(1, 1), numeric_lt(1, 2);
SELECT int4pl(2147483647, 1);
SELECT int4div(1, 0);
SELECT int4pl('1', '2');
