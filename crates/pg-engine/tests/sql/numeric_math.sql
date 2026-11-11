-- numeric transcendental functions: Postgres's result scales and digits
SELECT sqrt(2::numeric), sqrt(1e-20::numeric), sqrt(123456789012345678901234567890::numeric), sqrt(0.0000::numeric);
SELECT exp(1::numeric), exp(-1.5::numeric), exp(100::numeric), exp(0.000001::numeric), exp(-6000::numeric) = 0;
SELECT ln(2::numeric), ln(1.000001::numeric), ln(1e100::numeric), ln(0.5::numeric);
SELECT log(2::numeric), log(1000::numeric), log10(0.001::numeric), log(2, 1024::numeric), log(1.5, 12.25);
SELECT power(2::numeric, 0.5), power(2::numeric, -3), power(1.0001, 10000), power(10::numeric, -20), 2::numeric ^ 1000.0 > 1e300;
SELECT power(-2::numeric, 3), power(-8::numeric, 1.0), power(0::numeric, 0), power(0::numeric, 2.5), power(1.5::numeric, 'inf'), power(-1::numeric, '-inf');
SELECT power('-inf'::numeric, 3), power('-inf'::numeric, 2), power('inf'::numeric, -1), power('nan'::numeric, 0), power(1::numeric, 'nan');
SELECT power(-2::numeric, 0.5);
SELECT power(0::numeric, -1);
SELECT ln(0::numeric);
SELECT ln(-1::numeric);
SELECT log(1::numeric, 10);
SELECT log(-1::numeric);
SELECT exp(10000::numeric);
SELECT power(10::numeric, 131072);
SELECT sqrt(-1::numeric);

-- numeric input limits, rounding and typmods
SELECT '1e131071'::numeric > 0, '1e-16383'::numeric > 0, '1_000.000_1'::numeric, ' -0x1F '::numeric, '0b1_01'::numeric, '0o17'::numeric;
SELECT '1e131072'::numeric;
SELECT '1e-16384'::numeric;
SELECT '1e2147483648'::numeric;
SELECT '1__0'::numeric;
SELECT round(1234.5678, -2), round(-2.5::numeric), round(1e-20, 25), trunc(9999.99, -3), round(5e-1001, 1000), trunc(123.456, -100000);
SELECT 9.99::numeric(2,0);
SELECT 0.5::numeric(2,2), 0.995::numeric(2,2);
SELECT 123.45::numeric(4,-2), 123.45::numeric(2,-1);
SELECT 'inf'::numeric::numeric(3,1);
SELECT 1::numeric / 3, 2::numeric / 3.000, 1e-30 / 7, 10::numeric / 4, 12345678901234567890 / 0.0003;
SELECT 1e-10000 * 1e-10000 = 0, (1e-8000 * 1e-8000)::text = '0';
SELECT 9e131071 * 10;

-- width_bucket, gcd/lcm, factorial, scales
SELECT width_bucket(5.35, 0.024, 10.06, 5), width_bucket(-1::numeric, 0, 10, 3), width_bucket(11::numeric, 10, 0, 4), width_bucket(5::numeric, 10, 0, 4);
SELECT width_bucket(5.0::float8, 3.0, 4.0, 2147483647);
SELECT width_bucket('nan'::numeric, 1, 2, 3);
SELECT width_bucket(1::numeric, 1, 'inf', 3);
SELECT width_bucket(0.5::float8, 0, 1e308 * 10, 3);
SELECT width_bucket(1::float8, 1, 1, 3);
SELECT gcd(43312.5::numeric, 4637.5), lcm(4232.820::numeric, 132.72000), gcd('inf'::numeric, 0), lcm(0::numeric, 5);
SELECT factorial(20), factorial(0), scale(1.230::numeric), min_scale(1.230::numeric), trim_scale(1.2300::numeric);
SELECT numeric_inc(9.99), pg_lsn(23783416::numeric), pg_lsn(0.4::numeric);
SELECT pg_lsn(-1::numeric);

-- variance / stddev of numerics
SELECT variance(x), stddev_samp(x), var_pop(x), stddev_pop(x) FROM (VALUES (1.5), (2.25), (3.125)) v(x);
SELECT variance(x), var_pop(x) FROM (VALUES (1.50), (1.50)) v(x);
SELECT trim_scale(variance(a) * 1e1000) FROM (VALUES (0), (3e-500), (-3e-500), (4e-500 - 1e-16383), (-4e-500 + 1e-16383)) v(a);
SELECT variance(a) FROM (SELECT 9e131071 + x FROM generate_series(1, 5) x) v(a);

-- to_char / to_number on numbers
SELECT to_char(1234.5::numeric, '9G999D99'), to_char(-0.05::numeric, 'FM990.00'), to_char(1e20::numeric, '9.99EEEE'), to_char(485, 'RN'), to_char(12, 'FM99th');
SELECT to_char('inf'::float8, 'MI9999.99'), to_char('nan'::float4, '9999.99'), to_char('-inf'::numeric, '99.99'), to_char(4.2e9::float4, '9999999999.99');
SELECT to_number('123456', '99999V99'), to_number('1,234.5-', '9G999D9S'), to_number('<123>', '999PR'), to_char(123, '99V9');

-- float8 functions
SELECT sind(30), cosd(60), tand(45), cotd(45), asind(0.5), acosd(0.5), atand(1), atan2d(1, 1), sind(-210), cosd(240), tand(270);
-- (erfc / tgamma come from the platform libm; compare them rounded)
SELECT degrees(pi()), radians(180), erf(0.45), round(erfc(2.1)::numeric, 14), gamma(5), lgamma(1000), round(gamma(0.5)::numeric, 14);
SELECT gamma(0::float8);
SELECT lgamma(-1::float8);
SELECT gamma(-1000.5::float8);
SELECT exp(-1000::float8);
SELECT exp(1000::float8);
SELECT 'nan'::float8 / 0, 'nan'::float4 / 0::float4, float8send(1.5), float4send('7038531e-32'::float4), float4send('82381273e-35'::float4);
SELECT 1::float8 / 0;
SELECT covar_pop(y, x), covar_samp(y, x), corr(y, x), regr_slope(y, x), regr_r2(y, x) FROM (VALUES (1::float8, 'inf'::float8), (2, 3)) v(y, x);
SELECT float8_regr_accum('{4,140,2900,1290,83075,15050}'::float8[], 200, 100), float8_combine('{3,60,200}'::float8[], '{2,180,200}'::float8[]);
