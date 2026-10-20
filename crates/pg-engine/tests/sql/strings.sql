SELECT substr('hello', 2, 3), substring('hello' FROM 2 FOR 2), substring('hello' FROM 'l+'), left('hello', 2), right('hello', 2);
SELECT position('l' IN 'hello'), strpos('hello', 'lo'), replace('aaa', 'a', 'bb'), translate('hello', 'el', 'ip');
SELECT trim('  x  '), ltrim('xxyx', 'x'), rtrim('yxx', 'x'), btrim('--a--', '-'), trim(BOTH 'x' FROM 'xxaxx');
SELECT lpad('5', 3, '0'), rpad('ab', 5, '*'), repeat('ab', 3), reverse('abc'), initcap('hello world');
SELECT split_part('a,b,c', ',', 2), split_part('a,b,c', ',', -1), string_to_array('a,b,,c', ','), array_to_string(ARRAY['x', NULL, 'z'], '-', '?');
SELECT concat('a', NULL, 1), concat_ws('-', 'a', NULL, 'b'), format('%s is %s', 'x', 1), format('%I %L', 'Foo', 'it''s');
SELECT md5('abc'), sha256('abc'::bytea), encode('hi'::bytea, 'hex'), decode('6869', 'hex'), encode('hello', 'base64');
SELECT ascii('A'), chr(66), length('héllo'), octet_length('héllo'), bit_length('a'), char_length('abc');
SELECT 'abc' < 'abd', 'B' < 'a', upper('ß'), lower('ÀB');
SELECT regexp_replace('foo123bar', '[0-9]+', '#'), regexp_replace('aaa', 'a', 'b', 'g'), regexp_match('abc123', '([a-z]+)(\d+)'), regexp_matches('a1b2', '\d', 'g');
SELECT 'abc' ~* 'B', 'abc' !~ 'z', regexp_count('banana', 'an'), regexp_split_to_array('a1b22c', '\d+');
SELECT * FROM regexp_split_to_table('a b  c', '\s+') x;
SELECT starts_with('hello', 'he'), 'x' || 1 || true, quote_ident('Foo'), quote_literal('it''s'), quote_nullable(NULL);
SELECT to_hex(255), to_char(1234.5, '9999.99'), to_char(42, '00000'), to_number('1,234.5', '9,999.9');
SELECT 'a'::char(3) || 'b', length('a '::char(3)), 'abc'::varchar(2), E'tab\there', $$dollar 'quoted'$$;
SELECT overlay('Txxxxas' PLACING 'hom' FROM 2 FOR 4), 'abc' LIKE 'a\%', 'a%c' LIKE 'a\%c', 'ABC' NOT ILIKE 'abc';
SELECT string_agg(x, '') FROM unnest(ARRAY['a', 'b']) x;
SELECT left('abc', -1), repeat('x', 0), substr('abc', 0), substr('abc', 5);
SELECT lower(NULL), length(NULL::text), NULL || 'a';
SELECT 'x' * 2;
SELECT substr(123, 1, 2);

-- Set-returning regexp functions.
SELECT * FROM regexp_matches('foobarbequebaz', '(bar)(beque)');
SELECT regexp_matches('abcabc', '(b)(c)?', 'g');
SELECT * FROM regexp_split_to_table('hello world  x', '\s+') AS t(w);
SELECT regexp_split_to_table('a,B,c', ',', 'i');
SELECT * FROM unnest(ARRAY[1,2,3], ARRAY['a','b']) AS u(n, s);
SELECT 'x' || true, 'a'::char(3) || 'b', 1 || 'y', NULL::bool || 'z';
SELECT upper('straße'), lower('ΣΑΣ'), initcap('hello wORLD-foo bar_baz');
-- case mapping (builtin C.UTF-8: simple mappings) and case folding
SELECT lower('İ'), upper('ᾳᾀῃῳß'), initcap('ᾳbc ßa ǆungla'), casefold('ſςßµİꭰᏸΣAbc'), casefold('ẞ'), casefold('ϐϑϕϖϰϱϵẛι'), casefold(NULL);
