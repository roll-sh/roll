from pathlib import Path
import hashlib,json
P=Path(__file__).parent
HEADER="""\set ON_ERROR_STOP on
\set VERBOSITY verbose
\pset pager off
\pset format unaligned
\pset tuples_only on
SET client_encoding='UTF8';
SET statement_timeout='60s';
"""
CONFIG="""SET max_parallel_maintenance_workers={workers};
SET max_parallel_workers=1024;
SET min_parallel_table_scan_size=0;
SET min_parallel_index_scan_size=0;
SET maintenance_work_mem='128MB';
SELECT json_build_object('tag','settings','server_encoding',current_setting('server_encoding'),'client_encoding',current_setting('client_encoding'),'max_parallel_maintenance_workers',current_setting('max_parallel_maintenance_workers'),'max_parallel_workers',current_setting('max_parallel_workers'),'max_worker_processes',current_setting('max_worker_processes'),'min_parallel_table_scan_size',current_setting('min_parallel_table_scan_size'),'min_parallel_index_scan_size',current_setting('min_parallel_index_scan_size'),'maintenance_work_mem',current_setting('maintenance_work_mem'));
"""
FUNCTION="""CREATE FUNCTION {name}(n int) RETURNS text LANGUAGE sql IMMUTABLE AS $$
SELECT left(string_agg(md5(g::text), '' ORDER BY g), n)
FROM generate_series(1, (n+31)/32) g $$;
"""
def outcome(tag,sql):
 return f"\\echo BEGIN_RESULT {tag}\n\\set ON_ERROR_STOP off\n{sql};\n\\echo RESULT {tag} :SQLSTATE :ERROR\n\\set ON_ERROR_STOP on\n"
def save(name,sql):
 P.joinpath(name+'.sql').write_text(sql)
 return name
cases=[]
for workers in (0,2):
 for typ,n in [('minimum_asc',None),('minimum_desc',None),('include',2800),('include',8300),('duplicate',2800),('duplicate',8300),('duplicate',0)]:
  tag=f'{typ}'+('' if n is None else f'_{n}')+f'_w{workers}'
  if typ=='minimum_asc':
   key='CASE WHEN g=1 THEN 9000 WHEN g=300 THEN -10 WHEN g=600 THEN -20 ELSE g END'
   payload="CASE WHEN g=1 THEN ci_text(2800) WHEN g=300 THEN ci_text(3000) WHEN g=600 THEN ci_text(2880) ELSE 'small' END"
  elif typ=='minimum_desc':
   key='CASE WHEN g=1 THEN -9000 WHEN g=300 THEN 20 WHEN g=600 THEN 10 ELSE -g END'
   payload="CASE WHEN g=1 THEN ci_text(2800) WHEN g=300 THEN ci_text(2880) WHEN g=600 THEN ci_text(3000) ELSE 'small' END"
  else:
   key='CASE WHEN g=2 THEN 1 ELSE g END' if typ=='duplicate' else 'g'
   payload=f"CASE WHEN g=6000 THEN ci_text({n}) ELSE 'small' END"
  sql=HEADER+CONFIG.format(workers=workers)+FUNCTION.format(name='ci_text')+f"CREATE TABLE ci(k int, payload text) WITH (parallel_workers=2);\nINSERT INTO ci SELECT {key}, {payload} FROM generate_series(1,6000) g;\n"
  sql+="SELECT json_build_object('tag','prebuild_rows','ctid',ctid::text,'key',k,'actual_payload_bytes',octet_length(payload),'payload_characters',length(payload),'heap_stored_payload_bytes',pg_column_size(payload)) FROM ci WHERE octet_length(payload)>5 OR k IN (1,2) ORDER BY k;\nSELECT json_build_object('tag','prebuild_count','count',count(*)) FROM ci;\n"
  sql+=outcome(tag,f"CREATE {'UNIQUE ' if typ=='duplicate' else ''}INDEX ci_i ON ci(k{' DESC' if typ=='minimum_desc' else ''}) INCLUDE(payload)")
  sql+="SELECT json_build_object('tag','postbuild','count',count(*),'index_exists',to_regclass('ci_i') IS NOT NULL) FROM ci;\n"
  cases.append({'name':save(tag,sql),'db':tag,'kind':'build','workers':workers})

def stats(tag):
 return f"""SELECT pg_stat_force_next_flush();
SELECT pg_stat_clear_snapshot();
SELECT json_build_object('tag','stats','boundary','{tag}','counters',json_build_array(n_tup_ins,n_tup_upd,n_tup_del,n_live_tup,n_dead_tup,n_ins_since_vacuum)) FROM pg_stat_user_tables WHERE relname='cs';
SELECT json_build_object('tag','state','boundary','{tag}','count',count(*),'rows',json_agg(json_build_object('id',id,'payload',payload) ORDER BY id)) FROM cs;
"""
for n in (2800,8300):
 tag=f'stats_{n}'
 sql=HEADER+FUNCTION.format(name='cs_text')+"CREATE TABLE cs(id int PRIMARY KEY, payload text) WITH (autovacuum_enabled=off);\nCREATE INDEX cs_i ON cs(id) INCLUDE(payload);\nINSERT INTO cs VALUES (1, 'small');\n"+stats('initial')
 for step,statement in [('single',f'INSERT INTO cs VALUES (2, cs_text({n}))'),('multi',f"INSERT INTO cs VALUES (2, 'small'), (3, cs_text({n}))"),('small',"INSERT INTO cs VALUES (2, 'small')"),('update',f'UPDATE cs SET payload=cs_text({n}) WHERE id=1'),('upsert',f'INSERT INTO cs VALUES (1, cs_text({n})) ON CONFLICT(id) DO UPDATE SET payload=EXCLUDED.payload'),('speculative',f'INSERT INTO cs VALUES (5, cs_text({n})) ON CONFLICT DO NOTHING')]:
  if step in ('update','upsert','speculative'): sql+=stats('before_'+step)
  sql+=outcome(tag+'_'+step,statement)+stats(step)
 cases.append({'name':save(tag,sql),'db':tag,'kind':'stats','length':n})
for encoding in ('LATIN1','UTF8'):
 for workers in (0,2):
  tag=f'encoding_{encoding.lower()}_w{workers}'
  columns=', '.join(f'p{i} text' for i in range(16));include=', '.join(f'p{i}' for i in range(16));payload=', '.join(["repeat('é', 120)"]*16)
  sql=HEADER+CONFIG.format(workers=workers)+f'CREATE TABLE ce(k int, {columns}) WITH (parallel_workers=2);\n'
  sql+=''.join(f'ALTER TABLE ce ALTER COLUMN p{i} SET STORAGE PLAIN;\n' for i in range(16))
  sql+=f'INSERT INTO ce SELECT g, {payload} FROM generate_series(1,6000) g;\n'
  sql+="SELECT json_build_object('tag','encoding_prebuild','server_encoding',current_setting('server_encoding'),'client_encoding',current_setting('client_encoding'),'octet_length',octet_length(p0),'length',length(p0),'ctid',ctid::text,'key',k) FROM ce WHERE k=1;\nSELECT json_build_object('tag','column_storage','column',attname,'storage',attstorage::text) FROM pg_attribute WHERE attrelid='ce'::regclass AND attnum>1 AND NOT attisdropped ORDER BY attnum;\nSELECT json_build_object('tag','prebuild_count','count',count(*)) FROM ce;\n"
  sql+=outcome(tag,f'CREATE INDEX ce_i ON ce(k) INCLUDE({include})')
  sql+="SELECT json_build_object('tag','postbuild','count',count(*),'index_exists',to_regclass('ce_i') IS NOT NULL) FROM ce;\n"
  cases.append({'name':save(tag,sql),'db':tag,'kind':'encoding','workers':workers,'encoding':encoding})
admin=HEADER+"SELECT version();\nSHOW server_version_num;\nSELECT name,setting,unit,source FROM pg_settings WHERE name IN ('shared_buffers','maintenance_work_mem','max_parallel_workers','max_parallel_maintenance_workers','max_worker_processes','autovacuum','data_directory','lc_collate','lc_ctype','log_min_messages','max_connections');\n"
for c in cases: admin+=f"CREATE DATABASE {c['db']} TEMPLATE template0 ENCODING '{c.get('encoding','UTF8')}' LOCALE 'C';\n"
save('admin',admin)
P.joinpath('cases.json').write_text(json.dumps(cases,indent=2)+'\n')
P.joinpath('sql-sha256.json').write_text(json.dumps({p.name:hashlib.sha256(p.read_bytes()).hexdigest() for p in sorted(P.glob('*.sql'))},indent=2)+'\n')
print(f'Prepared {len(cases)} case scripts plus admin SQL')
