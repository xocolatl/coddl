#!/usr/bin/env sh
# examples/web-users/seed-db.sh
#
# Recreates users.sqlite with the demo data for the P4 web handler. The .sqlite
# file is gitignored; this script is the source of truth for the database
# contents. `active` is a Boolean, stored in SQLite as an INTEGER (0/1).
#
# Requires `sqlite3` on PATH.

set -e

cd "$(dirname "$0")"
rm -f users.sqlite

sqlite3 users.sqlite <<'SQL'
CREATE TABLE users (
    id     INTEGER NOT NULL,
    name   TEXT    NOT NULL,
    email  TEXT    NOT NULL,
    active INTEGER NOT NULL,

    PRIMARY KEY (id)
);

INSERT INTO users (id, name, email, active) VALUES
    (1, 'Alice', 'alice@example.com', 1),
    (2, 'Bob',   'bob@example.com',   0),
    (3, 'Carol', 'carol@example.com', 1),
    (4, 'Dave',  'dave@example.com',  0)
;
SQL
