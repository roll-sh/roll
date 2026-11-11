-- primary keys, unique, not null, check
CREATE TABLE org (id int PRIMARY KEY, name text NOT NULL UNIQUE, slug text, CONSTRAINT slug_ok CHECK (slug ~ '^[a-z]+$'));
INSERT INTO org VALUES (1, 'Acme', 'acme'), (2, 'Beta', NULL);
INSERT INTO org VALUES (1, 'Dup', 'dup');
INSERT INTO org VALUES (3, 'Acme', 'x');
INSERT INTO org VALUES (3, NULL, 'x');
INSERT INTO org VALUES (3, 'Gamma', 'Bad1');
INSERT INTO org (id, name) VALUES (3, 'Gamma') ON CONFLICT (id) DO NOTHING;
INSERT INTO org (id, name) VALUES (3, 'Gamma2') ON CONFLICT (id) DO UPDATE SET name = excluded.name RETURNING *;
INSERT INTO org (id, name) VALUES (4, 'Acme') ON CONFLICT ON CONSTRAINT org_name_key DO UPDATE SET slug = 'acme' RETURNING id, name, slug;
-- composite keys and unique nulls
CREATE TABLE member (org_id int, user_id int, role text DEFAULT 'member', PRIMARY KEY (org_id, user_id), UNIQUE NULLS NOT DISTINCT (org_id, role));
INSERT INTO member VALUES (1, 1, 'owner'), (1, 2, NULL);
INSERT INTO member VALUES (1, 3, NULL);
INSERT INTO member VALUES (1, 1, 'x');
-- foreign keys
CREATE TABLE team (id serial PRIMARY KEY, org_id int NOT NULL REFERENCES org ON DELETE CASCADE, lead_id int, name text);
CREATE TABLE task (id serial PRIMARY KEY, team_id int REFERENCES team (id) ON DELETE SET NULL ON UPDATE CASCADE, title text);
INSERT INTO team (org_id, name) VALUES (1, 'core'), (2, 'ops');
INSERT INTO team (org_id, name) VALUES (99, 'ghost');
INSERT INTO task (team_id, title) VALUES (1, 'a'), (2, 'b'), (NULL, 'c');
DELETE FROM org WHERE id = 2;
SELECT t.id, t.team_id, t.title FROM task t ORDER BY t.id;
UPDATE team SET id = 10 WHERE id = 1;
SELECT id, team_id FROM task ORDER BY id;
DELETE FROM team WHERE id = 10;
SELECT id, team_id FROM task ORDER BY id;
-- restrict and deferrable
CREATE TABLE parent (id int PRIMARY KEY);
CREATE TABLE child (id int PRIMARY KEY, parent_id int REFERENCES parent ON DELETE RESTRICT);
INSERT INTO parent VALUES (1);
INSERT INTO child VALUES (1, 1);
DELETE FROM parent WHERE id = 1;
CREATE TABLE child2 (id int, parent_id int REFERENCES parent DEFERRABLE INITIALLY DEFERRED);
BEGIN;
INSERT INTO child2 VALUES (1, 5);
INSERT INTO parent VALUES (5);
COMMIT;
BEGIN;
INSERT INTO child2 VALUES (2, 6);
COMMIT;
SELECT * FROM child2 ORDER BY id;
-- self reference and multi-column fk
CREATE TABLE node (id int PRIMARY KEY, parent int REFERENCES node);
INSERT INTO node VALUES (1, NULL), (2, 1), (3, 2);
DELETE FROM node WHERE id = 1;
CREATE TABLE assignment (org_id int, user_id int, FOREIGN KEY (org_id, user_id) REFERENCES member MATCH FULL);
INSERT INTO assignment VALUES (1, NULL);
INSERT INTO assignment VALUES (1, 1), (NULL, NULL);
-- alter table constraints
ALTER TABLE task ADD CONSTRAINT title_len CHECK (length(title) < 3);
ALTER TABLE task ADD CONSTRAINT title_len2 CHECK (length(title) < 3) NOT VALID;
INSERT INTO task (title) VALUES ('long');
ALTER TABLE task VALIDATE CONSTRAINT title_len2;
ALTER TABLE task DROP CONSTRAINT title_len2;
ALTER TABLE task ALTER COLUMN title SET NOT NULL;
UPDATE task SET title = NULL WHERE id = 1;
ALTER TABLE task ADD UNIQUE (title);
SELECT conname, contype, pg_get_constraintdef(oid) FROM pg_constraint WHERE conrelid IN ('task'::regclass, 'team'::regclass, 'member'::regclass) ORDER BY conrelid::regclass::text, conname;
SELECT tc.table_name, tc.constraint_name, tc.constraint_type, kcu.column_name, ccu.table_name AS ref_table, rc.update_rule, rc.delete_rule
FROM information_schema.table_constraints tc
JOIN information_schema.key_column_usage kcu ON kcu.constraint_name = tc.constraint_name AND kcu.table_name = tc.table_name
LEFT JOIN information_schema.referential_constraints rc ON rc.constraint_name = tc.constraint_name
LEFT JOIN information_schema.constraint_column_usage ccu ON ccu.constraint_name = rc.unique_constraint_name
WHERE tc.table_name IN ('team', 'task') ORDER BY 1, 2, 4;
DROP TABLE assignment, node, child2, child, parent, task, team, member, org;
