-- to_char of intervals at their limits: Postgres's int arithmetic wraps (-fwrapv) where the
-- engine overflowed (difftest seeds 5041, 5074, 5095).
SELECT iv, to_char(iv, 'YYYY MM DD'), to_char(iv, 'HH24:MI:SS.US'), to_char(iv, 'FMDD "days"'), to_char(iv, 'DDD')
FROM (VALUES (interval '178000000 years'), (interval '-178000000 years'), (interval '2147483647 days'),
             (interval '-2147483648 days'), (interval '2147483647 mons'), (interval '1 year -1 sec'),
             (interval '-2147483648 mons -2147483648 days')) v(iv);
SELECT to_char(interval '178000000 years', f) FROM unnest(ARRAY['Y,YYY', 'YYY', 'CC', 'IYYY', 'SSSS', 'HH12 AM']) f;
SELECT to_char(interval '178000000 years', 'Q');
SELECT to_char(interval '178000000 years', 'WW');
SELECT to_char(interval '178000000 years', 'J');
SELECT to_char(interval '-178000000 years -2147483648 days -2562047788 hours', f) FROM unnest(ARRAY['YYYY', 'DDD', 'HH24', 'SSSS', 'MI', 'US', 'CC']) f;
