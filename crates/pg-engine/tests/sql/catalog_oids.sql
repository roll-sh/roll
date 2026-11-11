-- built-in functions and operators carry Postgres's own oids
SELECT 'int4pl'::regproc::oid, 'textcat'::regproc::oid, 'now'::regproc::oid, 'length(text)'::regprocedure::oid, 'network_sub'::regproc::oid, 'bitand'::regproc::oid;
SELECT '+(int4,int4)'::regoperator::oid, '=(text,text)'::regoperator::oid, '||(text,text)'::regoperator::oid, '<<(inet,inet)'::regoperator::oid, '~(NONE,bit)'::regoperator::oid;
SELECT oid, oprname, oprleft::regtype, oprright::regtype, oprcode, oprcom::regoperator, oprnegate::regoperator, oprcanmerge, oprcanhash, oprrest, oprjoin FROM pg_operator WHERE oid IN (96, 98, 97, 521, 664, 931, 1784) ORDER BY oid;
SELECT count(*) FROM pg_operator WHERE oid >= 16384;
SELECT count(*) FROM pg_proc WHERE oid >= 16384;
SELECT p.oid, proname, pg_get_function_arguments(p.oid) FROM pg_proc p WHERE p.oid IN (177, 1258, 1299, 2116, 2147, 2803, 927) ORDER BY 1;
SELECT int4pl(1, 2), textcat('a', 'b'), network_sub('10.1.0.0/16', '10.0.0.0/8'), int8pl_inet(1, '1.2.3.4');
