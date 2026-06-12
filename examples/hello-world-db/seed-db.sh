#!/usr/bin/env sh
# examples/hello-world-db/seed-db.sh
#
# Recreates greetings.sqlite from scratch with the single hello-world
# row the example expects. The .sqlite file itself is gitignored; this
# script is the source of truth for what the database contains.
#
# Requires `sqlite3` on PATH.

set -e

cd "$(dirname "$0")"
rm -f greetings.sqlite

sqlite3 greetings.sqlite <<'SQL'
CREATE TABLE greetings (
    id INTEGER NOT NULL,
    message TEXT NOT NULL,

    PRIMARY KEY (id)
);

INSERT INTO greetings (id, message)
    VALUES (1, 'hello world')
;
SQL
