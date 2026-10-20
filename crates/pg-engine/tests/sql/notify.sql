-- LISTEN / NOTIFY (delivery is covered by the wire tests)
SELECT * FROM pg_listening_channels();
LISTEN chan_a;
LISTEN "Chan B";
LISTEN chan_a;
SELECT * FROM pg_listening_channels();
BEGIN;
LISTEN chan_c;
SELECT * FROM pg_listening_channels();
ROLLBACK;
SELECT * FROM pg_listening_channels();
BEGIN;
UNLISTEN chan_a;
SELECT * FROM pg_listening_channels();
COMMIT;
SELECT * FROM pg_listening_channels();
NOTIFY chan_a;
NOTIFY chan_a, 'payload';
SELECT pg_notify('chan_a', 'from function');
SELECT pg_notify('chan_a', NULL);
SELECT pg_notify('', 'x');
SELECT pg_notify(NULL, 'x');
SELECT pg_notify(repeat('c', 64), 'x');
SELECT pg_notify('chan_a', repeat('x', 8000));
NOTIFY chan_a, 'xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx';
SELECT pg_notification_queue_usage();
DO $$
BEGIN
  LISTEN chan_plpgsql;
  PERFORM pg_notify('chan_plpgsql', 'p');
  NOTIFY chan_plpgsql, 'q';
END $$;
SELECT * FROM pg_listening_channels();
UNLISTEN *;
SELECT * FROM pg_listening_channels();
LISTEN chan_d;
DISCARD ALL;
SELECT * FROM pg_listening_channels();
