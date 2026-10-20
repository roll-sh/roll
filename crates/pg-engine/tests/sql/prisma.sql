-- Prisma (schema-engine) introspection queries over a typical Prisma-migrated schema
CREATE TYPE "Role" AS ENUM ('USER', 'ADMIN', 'OWNER');
CREATE TABLE "User" (
    "id" TEXT NOT NULL,
    "email" TEXT NOT NULL,
    "name" VARCHAR(120),
    "role" "Role" NOT NULL DEFAULT 'USER',
    "tags" TEXT[] DEFAULT ARRAY[]::TEXT[],
    "meta" JSONB,
    "balance" DECIMAL(65,30) NOT NULL DEFAULT 0,
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,
    "updatedAt" TIMESTAMP(3) NOT NULL,
    CONSTRAINT "User_pkey" PRIMARY KEY ("id")
);
CREATE TABLE "Post" (
    "id" SERIAL NOT NULL,
    "title" TEXT NOT NULL,
    "published" BOOLEAN NOT NULL DEFAULT false,
    "authorId" TEXT,
    "views" BIGINT NOT NULL DEFAULT 0,
    "uuid" UUID NOT NULL DEFAULT gen_random_uuid(),
    CONSTRAINT "Post_pkey" PRIMARY KEY ("id"),
    CONSTRAINT "Post_views_check" CHECK ("views" >= 0)
);
CREATE TABLE "_PostToTag" ("A" INTEGER NOT NULL, "B" INTEGER NOT NULL, CONSTRAINT "_PostToTag_AB_pkey" PRIMARY KEY ("A", "B"));
CREATE UNIQUE INDEX "User_email_key" ON "User"("email");
CREATE INDEX "Post_authorId_published_idx" ON "Post"("authorId", "published" DESC);
CREATE INDEX "_PostToTag_B_index" ON "_PostToTag"("B");
ALTER TABLE "Post" ADD CONSTRAINT "Post_authorId_fkey" FOREIGN KEY ("authorId") REFERENCES "User"("id") ON DELETE SET NULL ON UPDATE CASCADE;
ALTER TABLE "_PostToTag" ADD CONSTRAINT "_PostToTag_A_fkey" FOREIGN KEY ("A") REFERENCES "Post"("id") ON DELETE CASCADE ON UPDATE CASCADE;
CREATE SEQUENCE "ticket_seq" START 100 INCREMENT 5;
CREATE VIEW "PublishedPost" AS SELECT "id", "title" FROM "Post" WHERE "published";
COMMENT ON TABLE "User" IS 'people';
COMMENT ON COLUMN "User"."email" IS 'login';
CREATE FUNCTION touch() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN NEW."updatedAt" := now(); RETURN NEW; END $$;
SELECT namespace.nspname as namespace_name
FROM pg_namespace as namespace
WHERE namespace.nspname = ANY ( ARRAY['public'] )
ORDER BY namespace_name;
SELECT
  tbl.relname AS table_name,
  namespace.nspname as namespace,
  (tbl.relhassubclass and tbl.relkind = 'p') as is_partition,
  (tbl.relhassubclass and tbl.relkind = 'r') as has_subclass,
  tbl.relrowsecurity as has_row_level_security,
  reloptions,
  obj_description(tbl.oid, 'pg_class') as description
FROM pg_class AS tbl
INNER JOIN pg_namespace AS namespace ON namespace.oid = tbl.relnamespace
WHERE
  (
    (tbl.relkind = 'r' AND tbl.relispartition = 'f')
      OR
    tbl.relkind = 'p'
  )
  AND namespace.nspname = ANY ( ARRAY['public'] )
ORDER BY namespace, table_name;
SELECT
                t.typname AS name,
                e.enumlabel AS value,
                n.nspname AS namespace,
                obj_description(t.oid, 'pg_type') AS description
            FROM pg_type t
            JOIN pg_enum e ON t.oid = e.enumtypid
            JOIN pg_catalog.pg_namespace n ON n.oid = t.typnamespace
            WHERE n.nspname = ANY ( ARRAY['public'] )
            ORDER BY e.enumsortorder;
SELECT
                ext.extname AS extension_name,
                ext.extversion AS extension_version,
                ext.extrelocatable AS extension_relocatable,
                pn.nspname AS extension_schema
            FROM pg_extension ext
            INNER JOIN pg_namespace pn ON ext.extnamespace = pn.oid
            ORDER BY ext.extname ASC;
SELECT p.proname AS name, n.nspname as namespace,
                CASE WHEN l.lanname = 'internal' THEN p.prosrc
                     ELSE pg_get_functiondef(p.oid)
                     END as definition
            FROM pg_proc p
            LEFT JOIN pg_namespace n ON p.pronamespace = n.oid
            LEFT JOIN pg_language l ON p.prolang = l.oid
            WHERE n.nspname = ANY ( ARRAY['public'] );
SELECT
                views.viewname AS view_name,
                views.definition AS view_sql,
                views.schemaname AS namespace,
                obj_description(class.oid, 'pg_class') AS description
            FROM pg_catalog.pg_views views
            INNER JOIN pg_catalog.pg_namespace ns ON views.schemaname = ns.nspname
            INNER JOIN pg_catalog.pg_class class ON class.relnamespace = ns.oid AND class.relname = views.viewname
            WHERE schemaname = ANY ( ARRAY['public'] );
SELECT
                oid.namespace,
                info.table_name,
                info.column_name,
                format_type(att.atttypid, att.atttypmod) as formatted_type,
                info.numeric_precision,
                info.numeric_scale,
                info.numeric_precision_radix,
                info.datetime_precision,
                info.data_type,
                info.udt_schema as type_schema_name,
                info.udt_name as full_data_type,
                pg_get_expr(attdef.adbin, attdef.adrelid) AS column_default,
                info.is_nullable,
                info.is_identity,
                info.character_maximum_length,
                col_description(att.attrelid, ordinal_position) AS description
            FROM information_schema.columns info
            JOIN pg_attribute att ON att.attname = info.column_name
            JOIN (
                 SELECT pg_class.oid, relname, pg_namespace.nspname as namespace
                 FROM pg_class
                 JOIN pg_namespace on pg_namespace.oid = pg_class.relnamespace
                 AND pg_namespace.nspname = ANY ( ARRAY['public'] )
                 WHERE reltype > 0
                ) as oid on oid.oid = att.attrelid
                  AND relname = info.table_name
                  AND namespace = info.table_schema
            LEFT OUTER JOIN pg_attrdef attdef ON attdef.adrelid = att.attrelid AND attdef.adnum = att.attnum AND table_schema = namespace
            WHERE table_schema = ANY ( ARRAY['public'] )
            ORDER BY namespace, table_name, ordinal_position;
SELECT
                (con.oid > 0)   AS "con_id",
                att2.attname    AS "child_column",
                cl.relname      AS "parent_table",
                att.attname     AS "parent_column",
                con.confdeltype,
                con.confupdtype,
                rel_ns.nspname  AS "referenced_schema_name",
                conname         AS constraint_name,
                child,
                parent,
                table_name,
                namespace,
                condeferrable,
                condeferred
            FROM (SELECT
                        ns.nspname AS "namespace",
                        unnest(con1.conkey)                AS "parent",
                        unnest(con1.confkey)                AS "child",
                        cl.relname                          AS table_name,
                        ns.nspname                          AS schema_name,
                        generate_subscripts(con1.conkey, 1) AS colidx,
                        con1.oid,
                        con1.confrelid,
                        con1.conrelid,
                        con1.conname,
                        con1.confdeltype,
                        con1.confupdtype,
                        con1.condeferrable                  AS condeferrable,
                        con1.condeferred                    AS condeferred
                FROM pg_class cl
                        join pg_constraint con1 on con1.conrelid = cl.oid
                        join pg_namespace ns on cl.relnamespace = ns.oid
                WHERE
                    ns.nspname = ANY ( ARRAY['public'] )
                    and con1.contype = 'f'
                ORDER BY colidx
                ) con
                    JOIN pg_attribute att on att.attrelid = con.confrelid and att.attnum = con.child
                    JOIN pg_class cl on cl.oid = con.confrelid
                    JOIN pg_attribute att2 on att2.attrelid = con.conrelid and att2.attnum = con.parent
                    JOIN pg_class rel_cl on con.confrelid = rel_cl.oid
                    JOIN pg_namespace rel_ns on rel_cl.relnamespace = rel_ns.oid
            ORDER BY namespace, table_name, constraint_name, con_id, con.colidx;
SELECT
                  sequencename AS sequence_name,
                  schemaname AS namespace,
                  start_value,
                  min_value,
                  max_value,
                  increment_by,
                  cycle,
                  cache_size
              FROM pg_sequences
              WHERE schemaname = ANY ( ARRAY['public'] )
              ORDER BY sequence_name;
SELECT
    schemainfo.nspname AS namespace,
    tableinfo.relname AS table_name,
	constr.conname AS constraint_name,
	constr.contype AS constraint_type,
	pg_get_constraintdef(constr.oid) AS constraint_definition,
	constr.condeferrable AS is_deferrable,
	constr.condeferred AS is_deferred
FROM pg_constraint constr
JOIN pg_class AS tableinfo
	ON tableinfo.oid = constr.conrelid
JOIN pg_namespace AS schemainfo
	ON schemainfo.oid = tableinfo.relnamespace
WHERE schemainfo.nspname = ANY ( ARRAY['public'] )
	AND contype NOT IN ('p', 'u', 'f')
ORDER BY namespace, table_name, constr.contype, constraint_name;
WITH rawindex AS (
    SELECT
        indrelid,
        indexrelid,
        indisunique,
        indisprimary,
        indpred,
        unnest(indkey) AS indkeyid,
        generate_subscripts(indkey, 1) AS indkeyidx,
        unnest(indclass) AS indclass,
        unnest(indoption) AS indoption
    FROM pg_index
    WHERE
        NOT indisexclusion
)
SELECT
    schemainfo.nspname AS namespace,
    indexinfo.relname AS index_name,
    tableinfo.relname AS table_name,
    columninfo.attname AS column_name,
    rawindex.indisunique AS is_unique,
    rawindex.indisprimary AS is_primary_key,
    rawindex.indkeyidx AS column_index,
    opclass.opcname AS opclass,
    opclass.opcdefault AS opcdefault,
    indexaccess.amname AS index_algo,
    CASE rawindex.indoption & 1
        WHEN 1 THEN 'DESC'
        ELSE 'ASC' END
        AS column_order,
    CASE rawindex.indoption & 2
        WHEN 2 THEN true
        ELSE false END
        AS nulls_first,
    pc.condeferrable AS condeferrable,
    pc.condeferred AS condeferred,
    pg_get_expr(rawindex.indpred, rawindex.indrelid) AS predicate
FROM
    rawindex
    INNER JOIN pg_class AS tableinfo ON tableinfo.oid = rawindex.indrelid
    INNER JOIN pg_class AS indexinfo ON indexinfo.oid = rawindex.indexrelid
    INNER JOIN pg_namespace AS schemainfo ON schemainfo.oid = tableinfo.relnamespace
    LEFT JOIN pg_attribute AS columninfo
        ON columninfo.attrelid = tableinfo.oid AND columninfo.attnum = rawindex.indkeyid
    INNER JOIN pg_am AS indexaccess ON indexaccess.oid = indexinfo.relam
    LEFT JOIN pg_opclass AS opclass
        ON opclass.oid = rawindex.indclass
    LEFT JOIN pg_constraint pc ON rawindex.indexrelid = pc.conindid AND pc.contype <> 'f'
WHERE schemainfo.nspname = ANY ( ARRAY['public'] )
ORDER BY namespace, table_name, index_name, column_index;
DROP VIEW "PublishedPost";
DROP TABLE "_PostToTag", "Post", "User";
DROP SEQUENCE "ticket_seq";
DROP TYPE "Role";
DROP FUNCTION touch();
