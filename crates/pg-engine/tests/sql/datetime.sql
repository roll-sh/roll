-- input / output
SELECT '2024-02-29'::date, '2024-02-29 13:45:06.123456'::timestamp, '13:45:06'::time;
SELECT '2024-02-29 13:45:06+02'::timestamptz;
SELECT 'epoch'::timestamp, 'infinity'::timestamp, '-infinity'::date;
SELECT 'Jan 5 2024'::date, '20240105'::date, '2024-1-5'::date;
SELECT '1 day 2 hours 3 minutes'::interval, '1 year 2 mons'::interval, '-1 day +02:00'::interval;
SELECT 'P1Y2M3DT4H5M6S'::interval, '1.5 days'::interval, '90 minutes'::interval;
SELECT '2024-02-30'::date;
SELECT '25:00:00'::time;
SELECT 'nonsense'::timestamp;
SELECT '24:00:00'::time, '2024-01-01 24:00:00'::timestamp;
SELECT 'today'::date = current_date, 'now'::timestamptz <= now();

-- arithmetic
SELECT date '2024-01-31' + 1, date '2024-03-01' - date '2024-02-01', date '2024-01-31' + interval '1 month';
SELECT timestamp '2024-01-31 10:00' + interval '1 month 1 day 1 hour';
SELECT timestamp '2024-03-01' - timestamp '2024-02-01 12:00';
SELECT interval '1 day' * 3, interval '3 hours' / 2, - interval '1 hour', interval '1 day' + interval '25 hours';
SELECT time '10:00' + interval '15 hours', time '23:00' - time '01:30';
SELECT date '2024-01-01' + time '10:30';
SELECT age(timestamp '2024-03-15', timestamp '2023-01-20');
SELECT justify_days(interval '35 days'), justify_hours(interval '27 hours'), justify_interval(interval '1 mon -1 hour');

-- extract / date_part / date_trunc
SELECT extract(year FROM timestamp '2024-07-04 12:34:56.789'), extract(month FROM date '2024-07-04'), extract(dow FROM date '2024-07-04');
SELECT extract(epoch FROM timestamp '2024-01-01'), extract(epoch FROM interval '1 day 1 hour');
SELECT extract(doy FROM date '2024-12-31'), extract(week FROM date '2024-12-31'), extract(isodow FROM date '2024-07-07'), extract(quarter FROM date '2024-07-04');
SELECT extract(microseconds FROM time '10:00:01.5'), extract(century FROM date '2000-01-01'), extract(millennium FROM date '2001-01-01');
SELECT date_part('hour', timestamp '2024-01-01 17:30'), date_part('minute', interval '2 hours 7 minutes');
SELECT date_trunc('month', timestamp '2024-07-04 12:34:56'), date_trunc('week', timestamp '2024-07-04 12:34:56'), date_trunc('quarter', date '2024-08-15');
SELECT date_trunc('hour', interval '3 days 4 hours 5 minutes'), date_trunc('year', timestamp '2024-07-04');
SELECT extract(foo FROM date '2024-01-01');

-- construction
SELECT make_date(2024, 2, 29), make_time(13, 45, 6.5), make_timestamp(2024, 2, 29, 13, 45, 6);
SELECT make_interval(years => 1, days => 2, mins => 3), make_interval(secs => 1.25), make_interval(1, 2), make_interval();
SELECT make_date(year => 2024, month => 2, day => 3), make_date(year => 2024, month => 2);
SELECT make_date(2023, 2, 29);
SELECT to_timestamp(0) AT TIME ZONE 'UTC', to_timestamp(1700000000.5) AT TIME ZONE 'UTC';

-- formatting
SELECT to_char(timestamp '2024-07-04 15:04:05.123', 'YYYY-MM-DD HH24:MI:SS.MS'), to_char(date '2024-07-04', 'Day, DD Month YYYY');
SELECT to_char(timestamp '2024-07-04 03:04:05', 'HH12:MI AM'), to_char(date '2024-07-04', 'FMDay FMMonth DDth'), to_char(interval '25 hours 3 minutes', 'HH24:MI');
SELECT to_char(date '2024-07-04', 'IYYY-IW-ID'), to_char(date '2024-07-04', 'Q WW J');
SELECT to_date('04 Jul 2024', 'DD Mon YYYY'), to_timestamp('2024-07-04 15:04', 'YYYY-MM-DD HH24:MI') AT TIME ZONE 'UTC';

-- time zones
SET TimeZone = 'UTC';
SELECT timestamptz '2024-07-04 12:00:00+00', timestamptz '2024-07-04 12:00:00 America/New_York';
SELECT timestamp '2024-07-04 12:00' AT TIME ZONE 'America/New_York', timestamptz '2024-07-04 12:00+00' AT TIME ZONE 'Asia/Tokyo';
SET TimeZone = 'America/New_York';
SELECT timestamptz '2024-01-15 12:00:00+00', timestamptz '2024-07-15 12:00:00+00';
SELECT timestamptz '2024-03-10 01:30' + interval '1 hour', timestamptz '2024-03-10 01:30' + interval '1 day';
SELECT extract(timezone FROM timestamptz '2024-07-15 12:00:00+00'), date_trunc('day', timestamptz '2024-07-15 02:00:00+00');
SELECT timestamptz '2024-07-15 12:00:00+00'::date, timestamptz '2024-07-15 12:00:00+00'::time, '2024-07-15'::date::timestamptz;
SET TimeZone = 'Europe/Budapest';
SELECT timestamptz '2024-10-27 00:30:00+00', timestamptz '2024-10-27 01:30:00+00';
SELECT date_trunc('day', timestamptz '2024-10-27 12:00:00+00', 'UTC');
RESET TimeZone;
SHOW TimeZone;

-- DateStyle / IntervalStyle
SET DateStyle = 'SQL, DMY';
SHOW DateStyle;
SELECT date '2024-07-04', timestamp '2024-07-04 15:04:05', timestamptz '2024-07-04 15:04:05+00', date '0044-03-15 BC';
SELECT '04/07/2024'::date, '04-07-24'::date;
SET DateStyle = 'SQL';
SHOW DateStyle;
SET DateStyle = 'SQL, MDY';
SELECT date '2024-07-04', timestamp '2024-07-04 15:04:05.5', '04/07/2024'::date;
SET DateStyle = 'German';
SHOW DateStyle;
SELECT date '2024-07-04', timestamp '2024-07-04 15:04:05', timestamptz '2024-07-04 15:04:05+00';
SET DateStyle = 'Postgres, MDY';
SELECT date '2024-07-04', timestamp '2024-07-04 15:04:05', timestamptz '2024-07-04 15:04:05+00', timestamp '0044-03-15 12:00 BC';
SET DateStyle = 'Postgres, DMY';
SELECT date '2024-07-04', timestamp '2024-07-07 15:04:05';
SET TimeZone = 'America/New_York';
SELECT timestamptz '2024-07-04 15:04:05+00', timestamptz '2024-01-04 15:04:05+00';
SET DateStyle = 'SQL, MDY';
SELECT timestamptz '2024-07-04 15:04:05+00';
SET TimeZone = 'Asia/Kolkata';
SELECT timestamptz '2024-07-04 15:04:05+00';
SET TimeZone = '+02';
SELECT timestamptz '2024-07-04 15:04:05+00';
RESET TimeZone;
SET DateStyle = 'ISO, YMD';
SELECT '24-07-04'::date, '2024/07/04'::date;
SET DateStyle = 'ISO, DMY';
SELECT '04/07/2024'::date, '13/01/2024'::date;
SET DateStyle = 'ISO, MDY';
SELECT '13/01/2024'::date;
SET DateStyle = 'ISO, SQL';
SET DateStyle = 'nonsense';
SET DateStyle = 'ISO, MDY';
SET IntervalStyle = 'iso_8601';
SELECT interval '1 year 2 months 3 days 04:05:06.5';
SET IntervalStyle = 'sql_standard';
SELECT interval '1 year 2 months', interval '3 days 04:05:06', interval '-1 day', interval '0';
SELECT interval '1 year 2 months 3 days', interval '-1 year -2 months', interval '-3 days -04:05:06.25', interval '1 day -1 hour', interval '04:05:06.5', interval '-00:00:01';
SET IntervalStyle = 'postgres_verbose';
SELECT interval '1 year 2 months 3 days 04:05:06';
SELECT interval '-1 year -2 days', interval '1 day -1 hour', interval '-1.5 seconds', interval '1 second', interval '0', interval '-1 day 02:00:00';
RESET IntervalStyle;
SELECT interval '-1 year -2 days', interval '1 day -1 hour', interval '-1.5 seconds';

-- comparisons, ranges, overlaps
SELECT date '2024-01-01' < timestamp '2024-01-01 00:00:01', timestamp '2024-01-01' = date '2024-01-01';
SELECT interval '1 day' = interval '24 hours', interval '1 month' = interval '30 days', interval '1 year' > interval '360 days';
SELECT (date '2024-01-01', date '2024-02-01') OVERLAPS (date '2024-01-15', date '2024-03-01');
SELECT greatest(date '2024-01-01', date '2023-01-01'), least(timestamp '2024-01-01', timestamp '2025-01-01');
SELECT isfinite(date 'infinity'), isfinite(timestamp '2024-01-01'), date 'infinity' > date '9999-12-31';

-- generate_series and tables
SELECT * FROM generate_series(timestamp '2024-01-01', timestamp '2024-01-03', interval '12 hours');
SELECT d::date FROM generate_series(date '2024-01-29', date '2024-02-02', interval '1 day') d;
CREATE TABLE ev (id int PRIMARY KEY, at timestamptz NOT NULL DEFAULT now(), d date, dur interval);
INSERT INTO ev (id, at, d, dur) VALUES (1, '2024-01-01 10:00+00', '2024-01-01', '1 hour'), (2, '2024-06-01 10:00+00', '2024-06-01', '2 days'), (3, '2024-12-31 23:59:59+00', NULL, NULL);
SELECT id, at, d, dur FROM ev ORDER BY at DESC;
SELECT date_trunc('month', at) AS m, count(*) FROM ev GROUP BY 1 ORDER BY 1;
SELECT sum(dur), avg(dur), min(at), max(d) FROM ev;
SELECT id FROM ev WHERE at BETWEEN '2024-01-01' AND '2024-07-01' ORDER BY id;
SELECT id, at + dur FROM ev WHERE dur IS NOT NULL ORDER BY id;
INSERT INTO ev (id) VALUES (4);
SELECT count(*) FROM ev WHERE at > now() - interval '1 minute';
DROP TABLE ev;
