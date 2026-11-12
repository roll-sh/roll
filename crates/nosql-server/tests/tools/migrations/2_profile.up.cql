CREATE TYPE address (street text, city text);
ALTER TABLE users ADD address frozen<address>;
ALTER TABLE users ADD tags set<text>;
