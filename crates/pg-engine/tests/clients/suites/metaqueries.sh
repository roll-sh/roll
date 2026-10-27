# pgAdmin 4 and DBeaver metadata queries, replayed (no GUI): pgAdmin 4 9.18's browser SQL
# templates (web/pgadmin/browser/**/templates/**/sql/*.sql, Jinja, the highest version
# directory up to 18_plus, the read-only ones), rendered with the a81_meta objects' oids the
# way its tree nodes pass them (database, schema, a table, a function, ...), and the metadata
# queries of DBeaver 26.2.3's PostgreSQL plugin (plugins/org.jkiss.dbeaver.ext.postgresql,
# model/PostgreSchema, PostgreDatabase, PostgreDataSource, PostgreDataTypeCache, PostgreRole,
# PostgreTableReal, PostgreProcedure, PostgreDependency, ...; written out below the way they
# are built for a PostgreSQL 18 server, `?` bound). Both run over metaqueries.setup.sql's
# database of many object kinds. One id per query, `pgadmin::<template>[@context]#<hash>` or
# `dbeaver::<reader>#<hash>`, the hash over its normalized result (column names and type oids,
# rows with user oids as catalog:name symbols and volatile columns masked, or SQLSTATE and
# message), so a query answering differently shows as one id per server; the texts are in
# $OUT/replay.txt (diff the two servers' files). Run it against a fresh server: databases and
# roles other suites leave behind show up in pgAdmin's database and role lists.
# image: python:3.12-slim
# src: https://github.com/pgadmin-org/pgadmin4/archive/refs/tags/REL-9_18.tar.gz
set -eu
V=/cache/venv/metaqueries
if [ ! -x $V/bin/python ]; then
  python -m venv $V
  $V/bin/pip install -q jinja2==3.1.6 psycopg==3.2.9 psycopg-binary==3.2.9
fi
W=/cache/work/metaqueries
rm -rf $W && mkdir -p $W
cat > $W/dbeaver.sql <<'SQL_EOF'
-- DBeaver 26.2.3 (plugins/org.jkiss.dbeaver.ext.postgresql, model/*.java) metadata queries,
-- built the way it builds them for a PostgreSQL 18 server, `?` bound to the a81_meta objects.
-- id: datasource.current_database
SELECT current_database()
-- id: datasource.version
SELECT version()
-- id: datasource.databases
SELECT db.oid,db.* FROM pg_catalog.pg_database db WHERE 1 = 1 AND datname = 'a81_meta'
-- id: datasource.templates
SELECT db.datname FROM pg_catalog.pg_database db WHERE datistemplate
-- id: datasource.settings_one
select * from pg_catalog.pg_settings where name='search_path'
-- id: database.by_name
SELECT db.oid,db.* FROM pg_catalog.pg_database db WHERE datname='a81_meta'
-- id: database.comment
select description from pg_shdescription join pg_database on objoid = pg_database.oid where datname = 'a81_meta'
-- id: database.types
SELECT t.oid,t.*,c.relkind,format_type(nullif(t.typbasetype, 0), t.typtypmod) as base_type_name, d.description
FROM pg_catalog.pg_type t
LEFT OUTER JOIN pg_catalog.pg_type et ON et.oid=t.typelem
LEFT OUTER JOIN pg_catalog.pg_class c ON c.oid=t.typrelid
LEFT OUTER JOIN pg_catalog.pg_description d ON t.oid=d.objoid
WHERE t.typname IS NOT NULL
AND (c.relkind IS NULL OR c.relkind = 'c') AND (et.typcategory IS NULL OR et.typcategory <> 'C')
-- id: database.roles
SELECT a.oid,a.*,pd.description FROM pg_catalog.pg_roles a
left join pg_catalog.pg_shdescription pd on a.oid = pd.objoid
ORDER BY a.rolname
-- id: database.access_methods
SELECT am.oid,am.* FROM pg_catalog.pg_am am
ORDER BY am.oid
-- id: database.encodings
SELECT c.contoencoding as encid,pg_catalog.pg_encoding_to_char(c.contoencoding) as encname
FROM pg_catalog.pg_conversion c
GROUP BY c.contoencoding
ORDER BY 2
-- id: database.collations
SELECT c.oid,c.* FROM pg_catalog.pg_collation c
ORDER BY c.oid
-- id: database.languages
SELECT l.oid,l.* FROM pg_catalog.pg_language l
ORDER BY l.oid
-- id: database.fdws
SELECT l.oid,l.*,p.pronamespace as handler_schema_id
FROM pg_catalog.pg_foreign_data_wrapper l
LEFT OUTER JOIN pg_catalog.pg_proc p ON p.oid=l.fdwhandler
ORDER BY l.fdwname
-- id: database.servers
SELECT l.oid,l.* FROM pg_catalog.pg_foreign_server l
ORDER BY l.srvname
-- id: database.tablespaces
SELECT t.oid,t.*,pg_tablespace_location(t.oid) loc
FROM pg_catalog.pg_tablespace t
ORDER BY t.oid
-- id: database.available_extensions
SELECT name,default_version,installed_version,comment FROM pg_catalog.pg_available_extensions WHERE name IN ('hstore', 'pg_trgm', 'file_fdw', 'plpgsql', 'citext') ORDER BY name
-- id: database.event_triggers
SELECT pet.*, d.description FROM pg_catalog.pg_event_trigger pet
LEFT OUTER JOIN pg_catalog.pg_description d ON pet."oid" = d.objoid
-- id: database.extensions
SELECT
 e.oid,
 cfg.tbls,
  n.nspname as schema_name,
 e.*
FROM
 pg_catalog.pg_extension e
 join pg_namespace n on n.oid =e.extnamespace
 left join  (
         select
            ARRAY_AGG(ns.nspname || '.' ||  cls.relname) tbls, oid_ext
          from
            (
            select
                unnest(e1.extconfig) oid , e1.oid oid_ext
            from
                pg_catalog.pg_extension e1 ) c
                join    pg_class cls on cls.oid = c.oid
                join pg_namespace ns on ns.oid = cls.relnamespace
            group by oid_ext
         ) cfg on cfg.oid_ext = e.oid
ORDER BY e.oid
-- id: database.schemas
SELECT n.oid,n.*,d.description FROM pg_catalog.pg_namespace n
LEFT OUTER JOIN pg_catalog.pg_description d ON d.objoid=n.oid AND d.objsubid=0 AND d.classoid='pg_namespace'::regclass
ORDER BY nspname
-- id: database.enums
SELECT * FROM pg_catalog.pg_enum
-- id: schema.inheritance
SELECT i.inhrelid relid, pc.relnamespace parent_ns, pc.oid parent_oid, i.inhseqno
FROM pg_catalog.pg_inherits i, pg_class rc, pg_class pc
WHERE rc.oid=i.inhrelid AND rc.relnamespace=${scid} AND pc.oid=i.inhparent
-- id: schema.by_name
SELECT s.oid as schema_id from pg_catalog.pg_namespace s WHERE s.nspname ='app'
-- id: schema.default_acl
SELECT * FROM pg_default_acl WHERE defaclnamespace = ${scid}
-- id: schema.extensions
SELECT
 e.oid,
 cfg.tbls,
 e.*
FROM
 pg_catalog.pg_extension e
 join pg_namespace n on n.oid =e.extnamespace
 left join  (
         select
            ARRAY_AGG(ns.nspname || '.' ||  cls.relname) tbls, oid_ext
          from
            (
            select
                unnest(e1.extconfig) oid , e1.oid oid_ext
            from
                pg_catalog.pg_extension e1 ) c
                join    pg_class cls on cls.oid = c.oid
                join pg_namespace ns on ns.oid = cls.relnamespace
            group by oid_ext
         ) cfg on cfg.oid_ext = e.oid

WHERE e.extnamespace=2200
ORDER BY e.oid
-- id: schema.aggregates
SELECT p.oid AS proc_oid,p.proname AS proc_name,a.*
FROM pg_catalog.pg_aggregate a,pg_catalog.pg_proc p
WHERE p.oid=a.aggfnoid AND p.pronamespace=${scid}
ORDER BY p.proname
-- id: schema.tables
SELECT c.oid,c.*,d.description,pg_catalog.pg_get_expr(c.relpartbound, c.oid) as partition_expr,  pg_catalog.pg_get_partkeydef(c.oid) as partition_key
FROM pg_catalog.pg_class c
LEFT OUTER JOIN pg_catalog.pg_description d ON d.objoid=c.oid AND d.objsubid=0 AND d.classoid='pg_class'::regclass
WHERE c.relnamespace=${scid} AND c.relkind not in ('i','I','c')
-- id: schema.columns
SELECT c.relname,a.*,pg_catalog.pg_get_expr(ad.adbin, ad.adrelid, true) as def_value,dsc.description,dep.objid
FROM pg_catalog.pg_attribute a
INNER JOIN pg_catalog.pg_class c ON (a.attrelid=c.oid)
LEFT OUTER JOIN pg_catalog.pg_attrdef ad ON (a.attrelid=ad.adrelid AND a.attnum = ad.adnum)
LEFT OUTER JOIN pg_catalog.pg_description dsc ON (c.oid=dsc.objoid AND a.attnum = dsc.objsubid)
LEFT OUTER JOIN pg_depend dep on dep.refobjid = a.attrelid AND dep.deptype = 'i' and dep.refobjsubid = a.attnum and dep.classid = dep.refclassid
WHERE NOT a.attisdropped AND c.relkind not in ('i','I','c') AND c.relnamespace=${scid}
ORDER BY a.attnum, c.relname
-- id: schema.constraints
SELECT c.oid,c.*,t.relname as tabrelname,rt.relnamespace as refnamespace,d.description, case when c.contype='c' then "substring"(pg_get_constraintdef(c.oid), 7) else null end consrc_copy
FROM pg_catalog.pg_constraint c
INNER JOIN pg_catalog.pg_class t ON t.oid=c.conrelid
LEFT OUTER JOIN pg_catalog.pg_class rt ON rt.oid=c.confrelid
LEFT OUTER JOIN pg_catalog.pg_description d ON d.objoid=c.oid AND d.objsubid=0 AND d.classoid='pg_constraint'::regclass
WHERE t.relnamespace=${scid}
ORDER BY c.oid
-- id: schema.indexes
SELECT i.*,i.indkey as keys,c.relname,c.relnamespace,c.relam,c.reltablespace,tc.relname as tabrelname,dsc.description,pg_catalog.pg_get_expr(i.indpred, i.indrelid) as pred_expr,pg_catalog.pg_get_expr(i.indexprs, i.indrelid, true) as expr,pg_catalog.pg_relation_size(i.indexrelid) as index_rel_size,pg_catalog.pg_stat_get_numscans(i.indexrelid) as index_num_scans
 FROM pg_catalog.pg_index i
INNER JOIN pg_catalog.pg_class c ON c.oid=i.indexrelid
INNER JOIN pg_catalog.pg_class tc ON tc.oid=i.indrelid
LEFT OUTER JOIN pg_catalog.pg_description dsc ON i.indexrelid=dsc.objoid
WHERE  c.relnamespace=${scid} ORDER BY tabrelname, c.relname
-- id: schema.procedures
SELECT p.oid as poid,p.*,pg_catalog.pg_get_expr(p.proargdefaults, 0) as arg_defaults,d.description
FROM pg_catalog.pg_proc p
LEFT OUTER JOIN pg_catalog.pg_description d ON d.objoid=p.oid and d.classoid='pg_proc'::regclass  AND d.objsubid = 0
WHERE p.pronamespace=${scid}
ORDER BY p.proname
-- id: schema.indexdef
select pg_catalog.pg_get_indexdef(i.indexrelid, k, true) from pg_index i, generate_series(0, 3) k WHERE i.indrelid IN (${parent}, ${child}) ORDER BY i.indexrelid::regclass::text, k
-- id: table.triggers.child
SELECT x.oid,x.*,p.pronamespace as func_schema_id,d.description
FROM pg_catalog.pg_trigger x
LEFT OUTER JOIN pg_catalog.pg_proc p ON p.oid=x.tgfoid
LEFT OUTER JOIN pg_catalog.pg_description d ON d.objoid=x.oid AND d.objsubid=0
WHERE x.tgrelid = ${child} AND NOT x.tgisinternal
-- id: table.rules.view
SELECT r.oid,r.*, pg_get_ruledef(r.oid) AS definition
FROM pg_rewrite r
WHERE r.ev_class=${view} AND r.rulename <> '_RETURN'::name
-- id: table.foreign
SELECT * FROM pg_catalog.pg_foreign_table WHERE ftrelid=${ext}
-- id: table.policies
SELECT * FROM pg_catalog.pg_policy WHERE polrelid = ${child}
-- id: table.triggerdef
SELECT pg_catalog.pg_get_triggerdef(oid) FROM pg_trigger WHERE tgrelid = ${child} AND NOT tgisinternal ORDER BY tgname
-- id: table.tgenabled
SELECT tgname, tgenabled FROM pg_catalog.pg_trigger WHERE tgrelid = ${child} ORDER BY tgname
-- id: role.members
SELECT * FROM pg_catalog.pg_auth_members WHERE member=${owner}
-- id: role.settings
select s.setconfig, pd.datname from pg_catalog.pg_db_role_setting s
left join pg_catalog.pg_database pd on s.setdatabase = pd.oid
where s.setrole = ${owner}
-- id: role.table_privileges
SELECT * FROM information_schema.table_privileges
WHERE table_catalog='a81_meta' AND grantee='meta_reader'
UNION ALL
SELECT
    pg_catalog.pg_get_userbyid(acl.grantor) AS grantor,
    pg_catalog.pg_get_userbyid(acl.grantee) AS grantee,
    current_database() AS table_catalog,
    n.nspname AS table_schema,
    c.relname AS table_name,
    acl.privilege_type,
    CASE WHEN acl.is_grantable THEN 'YES' ELSE 'NO' END AS is_grantable,
	'NO' AS with_hierarchy
FROM pg_catalog.pg_class c
JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
CROSS JOIN LATERAL pg_catalog.aclexplode(c.relacl) AS acl
WHERE c.relacl IS NOT NULL
  AND c.relkind IN ('r', 'v', 'f', 'p')
  AND acl.privilege_type in ('MAINTAIN')
  AND acl.grantee = ${owner}
-- id: role.routine_privileges
SELECT * FROM information_schema.routine_privileges WHERE specific_catalog='a81_meta' AND grantee='meta_reader'
-- id: role.namespace_acl
SELECT n.oid, n.nspacl FROM pg_catalog.pg_namespace n WHERE n.nspacl IS NOT NULL
-- id: role.object_grants
SELECT * FROM (
	SELECT DISTINCT relnamespace,
	relacl,
	relname,
	relkind,
(aclexplode(relacl)).grantee as granteeI
FROM
	pg_class
WHERE
	relacl IS NOT NULL
	AND relnamespace IN (
SELECT oid
FROM pg_namespace
WHERE nspname NOT LIKE 'pg_%' AND nspname != 'information_schema')
UNION ALL
SELECT DISTINCT
	n.oid AS relnamespace,
	nspacl AS relacl,
	n.nspname AS relname,
	cast('C' as "char") AS relkind,
(aclexplode(nspacl)).grantee as granteeI
FROM
	pg_catalog.pg_namespace n
WHERE
	n.nspacl IS NOT NULL
	) AS tr
WHERE granteeI=${owner}
-- id: role.default_privileges
SELECT DISTINCT g.* FROM (
SELECT *,
(aclexplode(defaclacl)).grantee as grantee
FROM pg_default_acl a WHERE a.defaclnamespace <> 0) as g
where grantee = 0 OR grantee IN (SELECT oid FROM pg_roles WHERE rolname LIKE 'meta%')
-- id: procedure.prosrc
SELECT prosrc FROM pg_proc where oid = ${touch}
-- id: procedure.functiondef
SELECT pg_get_functiondef(${fn})
-- id: procedure.functiondef_touch
SELECT pg_get_functiondef(${touch})
-- id: procedure.functiondef_proc
SELECT pg_get_functiondef(${proc})
-- id: procedure.aggregate
SELECT (pg_identify_object('pg_proc'::regclass, aggfnoid, 0)).identity,
aggtransfn::regproc,
format_type(aggtranstype, NULL) as aggtranstype,
CASE aggfinalfn WHEN '-'::regproc THEN NULL ELSE aggfinalfn::text END,
CASE aggsortop WHEN 0 THEN NULL ELSE oprname END,
agginitval, aggmtransfn, aggminvtransfn,
aggfinalextra, aggmfinalextra, aggserialfn, aggdeserialfn, aggmfinalfn,
format_type(aggmtranstype, NULL) as aggmtranstype
,aggfinalmodify, aggmfinalmodify FROM pg_aggregate
LEFT JOIN pg_operator ON pg_operator.oid = aggsortop
WHERE aggfnoid = 'app.sum_sq'::regproc
-- id: sequence.pg_sequences
SELECT * from pg_catalog.pg_sequences WHERE schemaname='app' AND sequencename='seq_a'
-- id: sequence.select
SELECT * from app.seq_a
-- id: view.viewdef
SELECT pg_get_viewdef(${view}, true)
-- id: view.viewdef_mview
SELECT pg_get_viewdef(${mview}, true)
-- id: index.indexdef
SELECT pg_catalog.pg_get_indexdef(indexrelid) FROM pg_index WHERE indrelid IN (${parent}, ${child}) ORDER BY 1
-- id: event_trigger.enabled
SELECT evtenabled FROM pg_catalog.pg_event_trigger WHERE oid=${evt}
-- id: datatype.enum_labels
SELECT e.enumlabel
FROM pg_catalog.pg_enum e
WHERE e.enumtypid=${mood}
ORDER BY e.enumsortorder
-- id: datatype.domain_constraints
SELECT concat(c.conname, ' ', pg_catalog.pg_get_constraintdef(oid, true))
FROM pg_catalog.pg_constraint c
WHERE contypid = ${posint}
-- id: datatype.composite_attributes
SELECT c.relname,row_number() OVER (ORDER BY a.attnum) as attnum,a.*,pg_catalog.pg_get_expr(ad.adbin, ad.adrelid, true) as def_value,dsc.description
FROM pg_catalog.pg_attribute a
INNER JOIN pg_catalog.pg_class c ON (a.attrelid=c.oid)
LEFT OUTER JOIN pg_catalog.pg_attrdef ad ON (a.attrelid=ad.adrelid AND a.attnum = ad.adnum)
LEFT OUTER JOIN pg_catalog.pg_description dsc ON (c.oid=dsc.objoid AND a.attnum = dsc.objsubid)
WHERE a.attnum > 0 AND NOT a.attisdropped AND c.oid=(SELECT typrelid FROM pg_type WHERE oid = 'app.pair'::regtype)
ORDER BY a.attnum
-- id: dependency.dependents.parent
SELECT DISTINCT dep.deptype, dep.classid, dep.objid, cl.relkind, attr.attname,pg_get_expr(ad.adbin, ad.adrelid) adefval,
    CASE WHEN cl.relkind IS NOT NULL THEN cl.relkind::text || COALESCE(dep.objsubid::text, '')::text
        WHEN tg.oid IS NOT NULL THEN 'T'::text
        WHEN ty.oid IS NOT NULL THEN 'y'::text
        WHEN ns.oid IS NOT NULL THEN 'n'::text
        WHEN pr.oid IS NOT NULL THEN 'p'::text
        WHEN la.oid IS NOT NULL THEN 'l'::text
        WHEN rw.oid IS NOT NULL THEN 'R'::text
        WHEN co.oid IS NOT NULL THEN 'C'::text || contype::text
        WHEN ad.oid IS NOT NULL THEN 'A'::text
        ELSE ''
    END AS type,
    COALESCE(coc.relname, clrw.relname, tgr.relname) AS ownertable,
    CASE WHEN cl.relname IS NOT NULL AND att.attname IS NOT NULL THEN cl.relname || '.' || att.attname
    ELSE COALESCE(cl.relname, co.conname, pr.proname, tg.tgname, ty.typname, la.lanname, rw.rulename, ns.nspname)
    END AS refname,
    COALESCE(nsc.nspname, nso.nspname, nsp.nspname, nst.nspname, nsrw.nspname, tgrn.nspname) AS nspname
FROM pg_depend dep
LEFT JOIN pg_class cl ON dep.objid=cl.oid
LEFT JOIN pg_attribute att ON dep.objid=att.attrelid AND dep.objsubid=att.attnum
LEFT JOIN pg_namespace nsc ON cl.relnamespace=nsc.oid
LEFT JOIN pg_proc pr ON dep.objid=pr.oid
LEFT JOIN pg_namespace nsp ON pr.pronamespace=nsp.oid
LEFT JOIN pg_trigger tg ON dep.objid=tg.oid
LEFT JOIN pg_class tgr ON tg.tgrelid=tgr.oid
LEFT JOIN pg_namespace tgrn ON tgr.relnamespace=tgrn.oid
LEFT JOIN pg_type ty ON dep.objid=ty.oid
LEFT JOIN pg_namespace nst ON ty.typnamespace=nst.oid
LEFT JOIN pg_constraint co ON dep.objid=co.oid
LEFT JOIN pg_class coc ON co.conrelid=coc.oid
LEFT JOIN pg_namespace nso ON co.connamespace=nso.oid
LEFT JOIN pg_rewrite rw ON dep.objid=rw.oid
LEFT JOIN pg_class clrw ON clrw.oid=rw.ev_class
LEFT JOIN pg_namespace nsrw ON clrw.relnamespace=nsrw.oid
LEFT JOIN pg_language la ON dep.objid=la.oid
LEFT JOIN pg_namespace ns ON dep.objid=ns.oid
LEFT JOIN pg_attrdef ad ON ad.oid=dep.objid
LEFT JOIN pg_attribute attr ON attr.attrelid=ad.adrelid and attr.attnum=ad.adnum
WHERE dep.refobjid=${parent}
ORDER BY type
-- id: dependency.dependencies.parent
SELECT DISTINCT dep.deptype, dep.classid, dep.refobjid, cl.relkind, attr.attname,pg_get_expr(ad.adbin, ad.adrelid) adefval,
    CASE WHEN cl.relkind IS NOT NULL THEN cl.relkind::text || COALESCE(dep.objsubid::text, '')::text
        WHEN tg.oid IS NOT NULL THEN 'T'::text
        WHEN ty.oid IS NOT NULL THEN 'y'::text
        WHEN ns.oid IS NOT NULL THEN 'n'::text
        WHEN pr.oid IS NOT NULL THEN 'p'::text
        WHEN la.oid IS NOT NULL THEN 'l'::text
        WHEN rw.oid IS NOT NULL THEN 'R'::text
        WHEN co.oid IS NOT NULL THEN 'C'::text || contype::text
        WHEN ad.oid IS NOT NULL THEN 'A'::text
        ELSE ''
    END AS type,
    COALESCE(coc.relname, clrw.relname, tgr.relname) AS ownertable,
    CASE WHEN cl.relname IS NOT NULL AND att.attname IS NOT NULL THEN cl.relname || '.' || att.attname
    ELSE COALESCE(cl.relname, co.conname, pr.proname, tg.tgname, ty.typname, la.lanname, rw.rulename, ns.nspname)
    END AS refname,
    COALESCE(nsc.nspname, nso.nspname, nsp.nspname, nst.nspname, nsrw.nspname, tgrn.nspname) AS nspname
FROM pg_depend dep
LEFT JOIN pg_class cl ON dep.refobjid=cl.oid
LEFT JOIN pg_attribute att ON dep.refobjid=att.attrelid AND dep.objsubid=att.attnum
LEFT JOIN pg_namespace nsc ON cl.relnamespace=nsc.oid
LEFT JOIN pg_proc pr ON dep.refobjid=pr.oid
LEFT JOIN pg_namespace nsp ON pr.pronamespace=nsp.oid
LEFT JOIN pg_trigger tg ON dep.refobjid=tg.oid
LEFT JOIN pg_class tgr ON tg.tgrelid=tgr.oid
LEFT JOIN pg_namespace tgrn ON tgr.relnamespace=tgrn.oid
LEFT JOIN pg_type ty ON dep.refobjid=ty.oid
LEFT JOIN pg_namespace nst ON ty.typnamespace=nst.oid
LEFT JOIN pg_constraint co ON dep.refobjid=co.oid
LEFT JOIN pg_class coc ON co.conrelid=coc.oid
LEFT JOIN pg_namespace nso ON co.connamespace=nso.oid
LEFT JOIN pg_rewrite rw ON dep.refobjid=rw.oid
LEFT JOIN pg_class clrw ON clrw.oid=rw.ev_class
LEFT JOIN pg_namespace nsrw ON clrw.relnamespace=nsrw.oid
LEFT JOIN pg_language la ON dep.refobjid=la.oid
LEFT JOIN pg_namespace ns ON dep.refobjid=ns.oid
LEFT JOIN pg_attrdef ad ON ad.oid=dep.refobjid
LEFT JOIN pg_attribute attr ON attr.attrelid=ad.adrelid and attr.attnum=ad.adnum
WHERE dep.objid=${parent}
ORDER BY type
SQL_EOF
cat > $W/replay.py <<'PY_EOF'
"""replay.py <target> <pgadmin web/pgadmin> <dbeaver queries file> <out dir>: pgAdmin 4's
browser SQL templates (PG 18 versions, read-only ones) rendered for the a81_meta objects, and
DBeaver's metadata queries, run against one server. Each query's normalized result (column
names and type oids, rows, or the error's SQLSTATE and message) goes to <out>/replay.txt; its
id, with a hash of that text, to <out>/meta.lst, so the two servers' runs compare id by id.
User oids (>= 16384) become catalog:name symbols; volatile columns (sizes, times, counters)
are masked."""
import hashlib
import os
import re
import sys

import jinja2
import psycopg
from jinja2 import meta

TARGET, PGA, DBV, OUT = sys.argv[1:5]
VER = re.compile(r"^(default|\d+_plus)$")
WRITE = re.compile(r"^(create|update|delete|drop|alter|grant|revoke|truncate|reset|enable|"
                   r"rename|move|attach|detach|refresh|insert|change|backend_support|sql|"
                   r"acl_|toggle|set_|vacuum|validate|clear|copy|run|lock|kill|terminate)")
NEEDS = {"data", "where", "where_clause", "condition", "keys", "fdwdata", "grant_reovke_sql"}
VOLATILE = re.compile(r"(size|xmin|xmax|xid|time|vacuum|analyze|scan|tup|blks|_read|_fetch|"
                      r"relpages|relfilenode|relallvisible|frozen|age$|lsn|pid|backend|"
                      r"stats_reset|xact|conflict|deadlock|temp_|checksum|session|last_value|"
                      r"log_cnt|is_called|tuples|rows|cost|version|reltuples)", re.I)
DSN = (f"host={os.environ['PGHOST']} port={os.environ['PGPORT']} user=postgres "
       f"password={os.environ['PGPASSWORD']} dbname=a81_meta")


def quote_ident(s):
    s = str(s)
    return s if re.fullmatch(r"[a-z_][a-z0-9_$]*", s) else '"' + s.replace('"', '""') + '"'


def qt_literal(v, conn=None, *a):
    if v is None or isinstance(v, jinja2.Undefined):
        return "NULL"
    if isinstance(v, bool):
        return "true" if v else "false"
    if isinstance(v, (int, float)):
        return str(v)
    if isinstance(v, (list, tuple)):
        return "ARRAY[" + ", ".join(qt_literal(x) for x in v) + "]"
    return "'" + str(v).replace("'", "''") + "'"


def qt_ident(conn, *names):
    return ".".join(quote_ident(n) for n in names if n and not isinstance(n, jinja2.Undefined))


def families():
    fam = {}
    for d, _, files in os.walk(os.path.join(PGA, "browser")):
        parts = d.split(os.sep)
        if "templates" not in parts or "ppas" in parts or "tests" in parts:
            continue
        vi = [i for i, p in enumerate(parts) if VER.match(p)]
        if not vi:
            continue
        i = vi[-1]
        num = 0 if parts[i] == "default" else int(parts[i].split("_")[0]) * 10000
        t = max(j for j, p in enumerate(parts) if p == "templates")
        for f in files:
            if num > 180000 or not f.endswith(".sql") or WRITE.match(f):
                continue
            key = "/".join(parts[t + 1:i] + parts[i + 1:] + [f])
            if key not in fam or fam[key][0] < num:
                fam[key] = (num, os.path.join(d, f), "/schemas/tables/" in d)
    return fam


class Norm:
    def __init__(self, conn):
        self.sym = {}
        cats = conn.execute("""SELECT c.relname FROM pg_class c JOIN pg_attribute a ON
            a.attrelid = c.oid AND a.attname = 'oid' WHERE c.relnamespace = 'pg_catalog'::regnamespace
            AND c.relkind = 'r' ORDER BY 1""").fetchall()
        for (cat,) in cats:
            cols = [r[0] for r in conn.execute(
                "SELECT attname FROM pg_attribute WHERE attrelid = %s::regclass AND attnum > 0 "
                "ORDER BY attnum", (f"pg_catalog.{cat}",))]
            label = {"pg_class": "oid::regclass::text", "pg_proc": "oid::regprocedure::text",
                     "pg_type": "oid::regtype::text", "pg_enum": "enumlabel"}.get(cat) or next(
                (c for c in cols if c.endswith("name")), None)
            sql = (f"SELECT oid, {label or 'NULL'}::text, row_number() OVER (ORDER BY oid) "
                   f"FROM pg_catalog.{cat} WHERE oid >= 16384")
            for oid, name, n in conn.execute(sql):
                self.sym.setdefault(int(oid), f"<{cat}:{name if name else '#' + str(n)}>")
        conn.rollback()

    def value(self, col, v):
        if v is None:
            return "NULL"
        if VOLATILE.search(col):
            return "<volatile>"
        if isinstance(v, int) and v in self.sym:
            return self.sym[v]
        s = repr(v) if not isinstance(v, str) else v
        return re.sub(r"\d{5,}", lambda m: self.sym.get(int(m.group()), m.group()), s)


def ids(conn):
    one = lambda s: conn.execute(s).fetchone()[0]  # noqa: E731
    r = dict(did=one("SELECT oid FROM pg_database WHERE datname = current_database()"),
             scid=one("SELECT 'app'::regnamespace::oid"))
    for k, s in [("parent", "'app.parent'::regclass"), ("child", "'app.child'::regclass"),
                 ("measure", "'app.measure'::regclass"), ("ext", "'app.ext_csv'::regclass"),
                 ("view", "'app.parent_v'::regclass"), ("mview", "'app.parent_mv'::regclass"),
                 ("posint", "'app.posint'::regtype"), ("mood", "'app.mood'::regtype"),
                 ("fn", "'app.add_one(int)'::regprocedure"), ("seq", "'app.seq_a'::regclass"),
                 ("fdw", "(SELECT oid FROM pg_foreign_data_wrapper WHERE fdwname = 'file_fdw')"),
                 ("srv", "(SELECT oid FROM pg_foreign_server WHERE srvname = 'files')"),
                 ("owner", "'meta_owner'::regrole"), ("proc", "'app.bump(int)'::regprocedure"),
                 ("touch", "'app.touch()'::regprocedure"), ("ci", "'app.ci'::regcollation"),
                 ("agg", "'app.sum_sq(int)'::regprocedure"), ("op", "'app.===(app.mood,app.mood)'::regoperator"),
                 ("pub", "(SELECT oid FROM pg_publication WHERE pubname = 'pub_app')"),
                 ("evt", "(SELECT oid FROM pg_event_trigger WHERE evtname = 'evt_ddl')"),
                 ("cfg", "'app.cfg'::regconfig"), ("dict", "'app.simple_d'::regdictionary"),
                 ("cast", "(SELECT oid FROM pg_cast WHERE castsource = 'app.pair'::regtype)"),
                 ("hstore", "(SELECT oid FROM pg_extension WHERE extname = 'hstore')"),
                 ("plpgsql", "(SELECT oid FROM pg_language WHERE lanname = 'plpgsql')"),
                 ("tsparser", "(SELECT oid FROM pg_ts_parser WHERE prsname = 'default')"),
                 ("tstmpl", "(SELECT oid FROM pg_ts_template WHERE tmplname = 'simple')")]:
        try:
            r[k] = one(f"SELECT {s}::oid")
        except psycopg.Error:
            conn.rollback()
            r[k] = 0
    for t in ("parent", "child", "measure"):
        for k, s in [("idx", "SELECT min(indexrelid) FROM pg_index WHERE indrelid = %s"),
                     ("trid", "SELECT min(oid) FROM pg_trigger WHERE tgrelid = %s AND NOT tgisinternal"),
                     ("plid", "SELECT min(oid) FROM pg_policy WHERE polrelid = %s"),
                     ("ptid", "SELECT min(inhrelid) FROM pg_inherits WHERE inhparent = %s"),
                     ("c", "SELECT min(oid) FROM pg_constraint WHERE conrelid = %s AND contype = 'c'"),
                     ("f", "SELECT min(oid) FROM pg_constraint WHERE conrelid = %s AND contype = 'f'"),
                     ("x", "SELECT min(oid) FROM pg_constraint WHERE conrelid = %s AND contype = 'x'"),
                     ("p", "SELECT min(oid) FROM pg_constraint WHERE conrelid = %s AND contype IN ('p', 'u')")]:
            r[f"{t}.{k}"] = conn.execute(s, (r[t],)).fetchone()[0] or 0
    conn.rollback()
    return r


OBJ = {"tables": ("tid", "parent"), "functions": ("fnid", "fn"), "procedures": ("fnid", "proc"),
       "trigger_functions": ("fnid", "touch"), "sequences": ("seid", "seq"),
       "types": ("tid", "mood"), "views": ("vid", "view"), "mviews": ("vid", "mview"),
       "domains": ("doid", "posint"), "collations": ("coid", "ci"),
       "foreign_tables": ("foid", "ext"), "publications": ("pbid", "pub"),
       "roles": ("rid", "owner"), "event_triggers": ("etid", "evt"),
       "fts_configurations": ("cfgid", "cfg"), "fts_dictionaries": ("dcid", "dict"),
       "fts_parsers": ("pid", "tsparser"), "fts_templates": ("tid", "tstmpl"),
       "aggregates": ("agid", "agg"), "operators": ("opid", "op"), "casts": ("cid", "cast"),
       "foreign_data_wrappers": ("fid", "fdw"), "foreign_servers": ("fsid", "srv"),
       "extensions": ("eid", "hstore"), "languages": ("lid", "plpgsql")}
CHILD = {"indexes": [("idx", "idx")], "triggers": [("trid", "trid")],
         "row_security_policies": [("plid", "plid")], "partitions": [("ptid", "ptid")],
         "check_constraint": [("cid", "c")], "foreign_key": [("fkid", "f"), ("cid", "f")],
         "exclusion_constraint": [("exid", "x"), ("cid", "x")],
         "index_constraint": [("cid", "p")]}


def contexts(key, is_table_child, o):
    base = dict(did=o["did"], scid=o["scid"], datlastsysoid=16383, last_system_oid=16383,
                show_system_objects=False, showsysobj=False, show_sys_objects=False,
                show_sysobj=False, server_type="pg", schema_diff=False, conn=None,
                fetch_database=True, fetch_dependents=True, _=lambda s: s)
    root = key.split("/")[0]
    extra = {"foreign_table_columns": dict(foid=o["ext"]), "domain_constraints":
             dict(doid=o["posint"]), "foreign_servers": dict(fid=o["fdw"]),
             "user_mappings": dict(fid=o["fdw"], fsid=o["srv"])}.get(root, {})
    if is_table_child and root != "tables":
        out = []
        for t in ("parent", "child", "measure"):
            out.append((t, dict(base, tid=o[t], **extra)))
            obj = {p: o[f"{t}.{k}"] for p, k in CHILD.get(root, [])}
            if obj:
                out.append((t + "+obj", dict(base, tid=o[t], **extra, **obj)))
        return out
    out = [("", dict(base, **extra))]
    if root in OBJ:
        out.append(("obj", dict(base, **extra, **{OBJ[root][0]: o[OBJ[root][1]]})))
    return out


def run(conn, norm, sql):
    ordered = bool(re.search(r"order\s+by", sql, re.I))
    try:
        with conn.cursor() as cur:
            cur.execute(sql)
            if cur.description is None:
                text = f"OK {cur.statusmessage}"
            else:
                cols = [(d.name, d.type_code) for d in cur.description]
                rows = ["|".join(norm.value(c[0], v) for c, v in zip(cols, row))
                        for row in cur.fetchall()]
                text = "cols " + ", ".join(f"{n}:{t}" for n, t in cols) + "\n" + "\n".join(
                    rows if ordered else sorted(rows))
    except psycopg.Error as e:
        text = f"ERROR {e.sqlstate}: {e.diag.message_primary}"
    conn.rollback()
    return text


def main():
    conn = psycopg.connect(DSN, cursor_factory=psycopg.ClientCursor)
    conn.execute("SET statement_timeout = '60s'")
    conn.commit()
    norm, o = Norm(conn), ids(conn)
    roots = sorted(d for d, _, _ in os.walk(PGA) if os.path.basename(d) == "templates")
    env = jinja2.Environment(loader=jinja2.FileSystemLoader(roots),
                             undefined=jinja2.ChainableUndefined, extensions=["jinja2.ext.do"])
    env.filters.update(qtIdent=qt_ident, qtTypeIdent=qt_ident, qtLiteral=qt_literal,
                       hasAny=lambda xs, k: any(k in x for x in (xs or [])))
    queries = []
    for key, (_, path, child) in sorted(families().items()):
        try:
            tpl_src = open(path).read()
            if meta.find_undeclared_variables(env.parse(tpl_src)) & NEEDS:
                continue
            tpl = env.from_string(tpl_src)
            for name, ctx in contexts(key, child, o):
                sql = tpl.render(**ctx).strip()
                if re.match(r"(?is)^(--[^\n]*\n|\s)*(select|with|show)\b", sql):
                    queries.append((f"pgadmin::{key}" + (f"@{name}" if name else ""), sql))
        except Exception as e:  # noqa: BLE001
            print("render", key, type(e).__name__, e)
    if os.path.exists(DBV):
        blocks = re.split(r"(?m)^-- id: ", open(DBV).read())[1:]
        for b in blocks:
            name, _, sql = b.partition("\n")
            sql = re.sub(r"\$\{(\w+)\}", lambda m: str(o[m.group(1)]), sql).strip()
            queries.append((f"dbeaver::{name.strip()}", sql))
    with open(f"{OUT}/replay.txt", "w") as rep, open(f"{OUT}/meta.lst", "w") as lst:
        for qid, sql in queries:
            text = run(conn, norm, sql)
            h = hashlib.sha1(text.encode()).hexdigest()[:10]
            lst.write(f"PASS {qid}#{h}\n")
            rep.write(f"== {qid}#{h}\n{text}\n")
    print(len(queries), "queries")


main()
PY_EOF
$V/bin/python -I $W/replay.py "$TARGET" "$SRC/web/pgadmin" $W/dbeaver.sql "$OUT"
