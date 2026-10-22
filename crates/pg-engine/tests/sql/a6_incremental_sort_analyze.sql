-- Incremental Sort under EXPLAIN ANALYZE: its full-sort and presorted groups, the sort
-- methods of each, and the input read only as far as the LIMIT above needs.

create table a6_t(a integer, b integer);

create or replace function a6_explain_analyze_without_memory(query text)
returns table (out_line text) language plpgsql
as
$$
declare
  line text;
begin
  for line in
    execute 'explain (analyze, costs off, summary off, timing off, buffers off) ' || query
  loop
    out_line := regexp_replace(line, '\d+kB', 'NNkB', 'g');
    return next;
  end loop;
end;
$$;

create or replace function a6_explain_analyze_inc_sort_nodes(query text)
returns jsonb language plpgsql
as
$$
declare
  elements jsonb;
  element jsonb;
  matching_nodes jsonb := '[]'::jsonb;
begin
  execute 'explain (analyze, costs off, summary off, timing off, buffers off, format ''json'') ' || query into strict elements;
  while jsonb_array_length(elements) > 0 loop
    element := elements->0;
    elements := elements - 0;
    case jsonb_typeof(element)
    when 'array' then
      if jsonb_array_length(element) > 0 then
        elements := elements || element;
      end if;
    when 'object' then
      if element ? 'Plan' then
        elements := elements || jsonb_build_array(element->'Plan');
        element := element - 'Plan';
      else
        if element ? 'Plans' then
          elements := elements || jsonb_build_array(element->'Plans');
          element := element - 'Plans';
        end if;
        if (element->>'Node Type')::text = 'Incremental Sort' then
          matching_nodes := matching_nodes || element;
        end if;
      end if;
    end case;
  end loop;
  return matching_nodes;
end;
$$;

create or replace function a6_explain_analyze_inc_sort_nodes_without_memory(query text)
returns jsonb language plpgsql
as
$$
declare
  nodes jsonb := '[]'::jsonb;
  node jsonb;
  group_key text;
  space_key text;
begin
  for node in select * from jsonb_array_elements(a6_explain_analyze_inc_sort_nodes(query)) t loop
    for group_key in select unnest(array['Full-sort Groups', 'Pre-sorted Groups']::text[]) t loop
      for space_key in select unnest(array['Sort Space Memory', 'Sort Space Disk']::text[]) t loop
        node := jsonb_set(node, array[group_key, space_key, 'Average Sort Space Used'], '"NN"', false);
        node := jsonb_set(node, array[group_key, space_key, 'Peak Sort Space Used'], '"NN"', false);
      end loop;
    end loop;
    nodes := nodes || node;
  end loop;
  return nodes;
end;
$$;

create or replace function a6_explain_analyze_inc_sort_nodes_verify_invariants(query text)
returns bool language plpgsql
as
$$
declare
  node jsonb;
  group_stats jsonb;
  group_key text;
  space_key text;
begin
  for node in select * from jsonb_array_elements(a6_explain_analyze_inc_sort_nodes(query)) t loop
    for group_key in select unnest(array['Full-sort Groups', 'Pre-sorted Groups']::text[]) t loop
      group_stats := node->group_key;
      for space_key in select unnest(array['Sort Space Memory', 'Sort Space Disk']::text[]) t loop
        if (group_stats->space_key->'Peak Sort Space Used')::bigint < (group_stats->space_key->'Peak Sort Space Used')::bigint then
          raise exception '% has invalid max space < average space', group_key;
        end if;
      end loop;
    end loop;
  end loop;
  return true;
end;
$$;

insert into a6_t(a, b) select i/50 + 1, i + 1 from generate_series(0, 999) n(i);
analyze a6_t;
select a6_explain_analyze_without_memory('select * from (select * from a6_t order by a) s order by a, b limit 55');
select jsonb_pretty(a6_explain_analyze_inc_sort_nodes_without_memory('select * from (select * from a6_t order by a) s order by a, b limit 55'));
select a6_explain_analyze_inc_sort_nodes_verify_invariants('select * from (select * from a6_t order by a) s order by a, b limit 55');
delete from a6_t;
insert into a6_t(a, b) select (case when i < 5 then i else 9 end), i from generate_series(1, 1000) n(i);
analyze a6_t;
select a6_explain_analyze_without_memory('select * from (select * from a6_t order by a) s order by a, b limit 70');
select jsonb_pretty(a6_explain_analyze_inc_sort_nodes_without_memory('select * from (select * from a6_t order by a) s order by a, b limit 70'));
select * from (select * from a6_t order by a) s order by a, b limit 5;
select a6_explain_analyze_without_memory('select * from (select * from a6_t order by a) s order by a, b limit 5');
select a6_explain_analyze_without_memory('select * from (select * from a6_t order by a) s order by a, b');
drop table a6_t;
drop function a6_explain_analyze_without_memory(text);
drop function a6_explain_analyze_inc_sort_nodes(text);
drop function a6_explain_analyze_inc_sort_nodes_without_memory(text);
drop function a6_explain_analyze_inc_sort_nodes_verify_invariants(text);
