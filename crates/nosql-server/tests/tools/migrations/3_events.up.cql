CREATE TABLE events (user_id uuid, at timeuuid, kind text, PRIMARY KEY (user_id, at)) WITH CLUSTERING ORDER BY (at DESC);
INSERT INTO events (user_id, at, kind) VALUES (00000000-0000-0000-0000-000000000001, now(), 'migrated');
