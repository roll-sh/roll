-- Shortest float output where the shortest decimal is exactly halfway up to the next float:
-- Ryu doesn't take it, so a longer decimal is printed.
select x::float4 from (values (33554448), (8999999488), (34366717952)) v(x);
select '305404.12'::float4, '8099.0312'::float4, '0.00024414062'::float4, '0.0024414062'::float4, '0.0043945312'::float4, '0.0063476562'::float4;
select x from (values ('9.999999999999999e+22'::float8), ('1.8014398509481992e+16'), ('1.8014398509529112e+16'), ('2.0266198323167248e+16'), ('2.7021597764222992e+16'), ('3.4902897112121472e+16'), ('-1.8014398509481992e+16')) v(x);
select 1e23::float8, 0.1::float8, 0.5::float4, 3.0::float8, 1e-45::float4, 123456789012345.6::float8;
