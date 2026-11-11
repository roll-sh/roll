CREATE TABLE p (id int PRIMARY KEY);
CREATE TABLE c (id int, pid int CONSTRAINT c_pid_fk REFERENCES p DEFERRABLE);
CREATE TABLE d (id int, pid int, CONSTRAINT d_pid_fk FOREIGN KEY (pid) REFERENCES p DEFERRABLE INITIALLY DEFERRED);
CREATE TABLE n (id int, pid int CONSTRAINT n_pid_fk REFERENCES p);
SELECT conname, condeferrable, condeferred, pg_get_constraintdef(oid) FROM pg_constraint WHERE contype = 'f' ORDER BY conname;
SELECT constraint_name, is_deferrable, initially_deferred FROM information_schema.table_constraints WHERE constraint_type = 'FOREIGN KEY' ORDER BY 1;
-- deferrable but immediate by default
INSERT INTO c VALUES (1, 1);
BEGIN;
SET CONSTRAINTS c_pid_fk DEFERRED;
INSERT INTO c VALUES (1, 1);
INSERT INTO p VALUES (1);
COMMIT;
-- SET CONSTRAINTS IMMEDIATE checks pending rows right away
BEGIN;
INSERT INTO d VALUES (1, 2);
SET CONSTRAINTS d_pid_fk IMMEDIATE;
ROLLBACK;
BEGIN;
INSERT INTO d VALUES (1, 2);
INSERT INTO p VALUES (2);
SET CONSTRAINTS ALL IMMEDIATE;
INSERT INTO d VALUES (2, 3);
COMMIT;
-- parent side: delete then re-insert the key before commit
BEGIN;
SET CONSTRAINTS ALL DEFERRED;
DELETE FROM p WHERE id = 1;
INSERT INTO p VALUES (1);
COMMIT;
BEGIN;
SET CONSTRAINTS ALL DEFERRED;
DELETE FROM p WHERE id = 1;
COMMIT;
-- savepoints drop checks queued after them
BEGIN;
INSERT INTO d VALUES (3, 1);
SAVEPOINT s;
INSERT INTO d VALUES (4, 99);
ROLLBACK TO s;
COMMIT;
-- rows deleted before commit are not checked
BEGIN;
INSERT INTO d VALUES (5, 77);
DELETE FROM d WHERE id = 5;
COMMIT;
-- implicit transactions check at the end of the statement
INSERT INTO d VALUES (6, 55);
SET CONSTRAINTS ALL DEFERRED;
BEGIN;
SET CONSTRAINTS n_pid_fk DEFERRED;
ROLLBACK;
BEGIN;
SET CONSTRAINTS nope DEFERRED;
ROLLBACK;
SELECT * FROM d ORDER BY id;
SELECT * FROM c ORDER BY id;
DROP TABLE c, d, n, p;
