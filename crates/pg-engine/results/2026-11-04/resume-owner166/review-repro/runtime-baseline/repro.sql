SET a166.bad_oid = 'not_an_oid';
SELECT count(*) AS no_qualified_rows FROM pg_class WHERE relkind = 'x' AND relnamespace = current_setting('a166.bad_oid')::oid;
SELECT count(*) AS runtime_bad_oid FROM pg_class WHERE relnamespace = current_setting('a166.bad_oid')::oid;
