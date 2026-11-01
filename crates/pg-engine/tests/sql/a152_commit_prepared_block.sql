-- COMMIT / ROLLBACK PREPARED refuse a multi-statement query string (an implicit
-- transaction block), as PreventInTransactionBlock does.
COMMIT PREPARED 'a152_nosuch' \; SELECT 1;
ROLLBACK PREPARED 'a152_nosuch' \; SELECT 1;
COMMIT PREPARED 'a152_nosuch';
