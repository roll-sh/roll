-- COPY TO in CSV quotes an empty string only when it is the NULL marker (CopyAttributeOutCSV);
-- difftest seed 5002 #95.
COPY (SELECT ''::text AS a, 'x' AS b, NULL::text AS c) TO STDOUT (FORMAT csv);
COPY (SELECT ''::text AS a, 'x' AS b, NULL::text AS c) TO STDOUT (FORMAT csv, NULL 'N');
COPY (SELECT ''::text AS a, 'N' AS b, NULL::text AS c) TO STDOUT (FORMAT csv, NULL 'N', HEADER);
COPY (SELECT ''::tsvector AS a, ' ' AS b) TO STDOUT (FORMAT csv, DELIMITER ';', NULL 'N');
COPY (SELECT ''::text AS a) TO STDOUT (FORMAT csv, NULL 'N');
COPY (SELECT ''::text AS a, 'q' AS b) TO STDOUT (FORMAT csv, FORCE_QUOTE (a), NULL 'N');
COPY (SELECT ''::text AS a, 'q' AS b) TO STDOUT (FORMAT text, NULL 'N');
