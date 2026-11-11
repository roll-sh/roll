-- Found by the differential tester (tests/difftest.rs). cbrt() is the C library's; the reference
-- PostgreSQL uses musl's, whose last digit differs from the host libm for some inputs.
SELECT cbrt(3.75::float8), cbrt(27), cbrt(-8), cbrt(0), cbrt(-0::float8), cbrt(1e-310::float8), cbrt('Infinity'::float8), cbrt('NaN'::float8), cbrt(2), cbrt(10), cbrt(0.001), cbrt(123456789.123), ||/ 64::float8;
SELECT x, cbrt(x::float8) FROM generate_series(1, 40) x;
SELECT x, cbrt(x::float8 / 7) FROM generate_series(1, 40) x;
SELECT cbrt(1e300), cbrt(-1e-300), cbrt(5e-324);
