-- time zones: names, POSIX rules, abbreviations, DST transitions
SET TimeZone = 'America/New_York';
SELECT '2024-03-10 02:30'::timestamptz, '2024-11-03 01:30'::timestamptz, '2024-11-03 01:30 EDT'::timestamptz;
SELECT timestamptz '2024-07-01 12:00+00' AT TIME ZONE 'Europe/Berlin', timestamptz '2024-01-01 12:00+00' AT TIME ZONE 'Asia/Kolkata';
SELECT timestamp '2024-07-01 12:00' AT TIME ZONE 'Australia/Sydney', timestamp '2024-07-01 12:00' AT TIME ZONE 'UTC+3';
SELECT '2024-06-01 10:00 PST8PDT'::timestamptz, '2024-06-01 10:00 EST5EDT'::timestamptz, '2024-06-01 10:00 MSK'::timestamptz;
SELECT timezone('Europe/London', timestamptz '1970-06-01 00:00+00'), timezone('Europe/London', timestamptz '2100-06-01 00:00+00');
SELECT '1900-01-01 00:00'::timestamptz, '1883-11-18 12:00'::timestamptz;
SET TimeZone = 'Europe/Moscow';
SELECT '2010-06-01'::timestamptz, '2014-12-01'::timestamptz, '2011-03-27 02:30'::timestamptz;
SET TimeZone = '+05:30';
SELECT '2024-01-01 00:00'::timestamptz, current_setting('TimeZone');
SET TimeZone = 'UTC';
SELECT '2024-01-01 00:00 Europe/Paris'::timestamptz, '2024-07-01 00:00 America/Sao_Paulo'::timestamptz;
SELECT 'J2451545'::date, '2024-01-01 allballs'::timestamp, 'epoch'::timestamptz;
SET TimeZone = 'No/Such_Zone';
SELECT '2024-01-01 00:00 No/Such_Zone'::timestamptz;
SET timezone_abbreviations = 'Australia';
SELECT '2024-01-01 10:00 EST'::timestamptz, '2024-01-01 10:00 CST'::timestamptz;
RESET timezone_abbreviations;
SELECT '2024-01-01 10:00 EST'::timestamptz;

-- date styles and output
SET DateStyle = 'Postgres, DMY';
SELECT timestamptz '2024-02-29 13:45:06.5+00', date '2024-02-29', '01/02/2024'::date;
SET DateStyle = 'SQL, MDY';
SELECT timestamptz '2024-02-29 13:45:06.5+00', date '2024-02-29', '01/02/2024'::date;
SET DateStyle = 'German';
SELECT timestamptz '2024-02-29 13:45:06.5+00', date '0044-03-15 BC';
SET DateStyle = 'ISO, YMD';
SELECT '24-02-29'::date, timestamptz '2024-02-29 13:45:06.5+00';
RESET DateStyle;

-- interval styles
SET IntervalStyle = 'sql_standard';
SELECT interval '1 year 2 mons 3 days 04:05:06.7', interval '-1 day 2 hours', interval '1-2', interval '3 4:05:06';
SET IntervalStyle = 'postgres_verbose';
SELECT interval '1 year 2 mons 3 days 04:05:06.7', interval '-1 day 2 hours', interval '-00:00:01.5';
SET IntervalStyle = 'iso_8601';
SELECT interval '1 year 2 mons 3 days 04:05:06.7', interval '-1 day 2 hours', interval '0';
RESET IntervalStyle;
SELECT interval '1 day 2 hours' minute to second, interval '123456789 years', interval '2147483647 days' + interval '1 day';
SELECT interval 'P0.5Y', interval 'PT1.5H', interval '@ 1 day ago', interval '1.5 weeks';

-- to_char / to_timestamp / to_date
SELECT to_char(timestamp '2024-07-04 13:05:09.123456', 'YYYY-MM-DD HH24:MI:SS.US Day DY Month MON IW IYYY Q WW J');
SELECT to_char(timestamp '2024-07-04 13:05:09', 'FMDay, FMDDth "of" FMMonth YYYY HH12:MI AM'), to_char(date '0001-01-01' - 1, 'YYYY BC');
SELECT to_char(interval '1 day 25:61:61', 'DD HH24:MI:SS'), to_char(timestamptz '2024-07-04 13:05:09+00', 'TZ OF TZH:TZM');
SELECT to_timestamp('2024-07-04 13:05:09.5 +02', 'YYYY-MM-DD HH24:MI:SS.FF1 TZH'), to_timestamp('04 Jul 2024', 'DD Mon YYYY');
SELECT to_date('2024 185', 'YYYY DDD'), to_date('2024-W27-4', 'IYYY-"W"IW-ID'), to_timestamp(1720098309.5);
SELECT to_date('2024-13-01', 'YYYY-MM-DD');
SELECT to_timestamp('2024-07-04 25:00', 'YYYY-MM-DD HH24:MI');

-- extract / date_bin / overflow errors
SELECT extract(epoch FROM timestamptz '2024-07-04 13:05:09.5+02'), extract(timezone_hour FROM timestamptz '2024-07-04 00:00+00'), extract(julian FROM date '2024-07-04');
SELECT date_bin('15 minutes', timestamp '2024-07-04 13:07:09', timestamp '2000-01-01'), date_trunc('day', timestamptz '2024-07-04 23:30+00', 'Asia/Tokyo');
SELECT timestamp '294276-12-31 23:59:59' + interval '1 second';
SELECT date '5874897-12-31' + 1;
SELECT make_interval(years => 2147483647);
SELECT '12:00'::time(7), '2024-01-01 12:00:00.1234567'::timestamp(9), localtimestamp(8) IS NOT NULL, current_time(7) IS NOT NULL;
SELECT '12:00'::timestamptz(-1);
SELECT make_timestamptz(2024, 7, 4, 13, 5, 9.5, 'America/Chicago'), make_date(-44, 3, 15);
SELECT age(timestamptz '2024-03-31 00:00+00', timestamptz '2024-02-29 12:00+00'), interval '1 month' / 3, interval '1 hour' * 1.5;

-- jsonpath datetime methods
SELECT jsonb_path_query('"2017-03-10 12:34:56+3:10"', '$.datetime()'), jsonb_path_query('"12:34:56.789Z"', '$.datetime().type()');
SELECT jsonb_path_query_tz('"2023-08-15 12:34:56 +05:30"', '$.timestamp()'), jsonb_path_query('"2023-08-15"', '$.timestamp(2).string()');
SELECT jsonb_path_query('"10-03-2017 12:34"', '$.datetime("dd-mm-yyyy HH24:MI")'), jsonb_path_query('"12:34:56.789"', '$.time(10)');
SELECT jsonb_path_query('"2023-08-15 12:34:56"', '$.timestamp_tz()');
SELECT to_jsonb(timestamptz '2024-07-04 13:05:09+00'), to_json(date 'infinity'), to_jsonb(time '10:00:00.5');
