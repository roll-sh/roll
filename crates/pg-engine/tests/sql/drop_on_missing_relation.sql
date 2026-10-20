CREATE TABLE gx_dr_te (a int);
DROP TRIGGER IF EXISTS gx_dr_trg ON gx_dr_no_such_table;
DROP TRIGGER IF EXISTS gx_dr_trg ON gx_dr_no_such_schema.gx_dr_no_such_table;
DROP TRIGGER gx_dr_trg ON gx_dr_no_such_schema.gx_dr_no_such_table;
DROP TRIGGER IF EXISTS gx_dr_trg ON gx_dr_te;
DROP POLICY IF EXISTS gx_dr_pol ON gx_dr_no_such_table;
DROP POLICY IF EXISTS gx_dr_pol ON gx_dr_no_such_schema.gx_dr_no_such_table;
DROP POLICY IF EXISTS gx_dr_pol ON gx_dr_te;
DROP TABLE gx_dr_te;
