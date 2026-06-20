#!/usr/bin/env sh
# examples/union-intersect-minus/seed-db.sh
#
# Recreates shifts.sqlite with the demo data shared with the in-process
# union-intersect-minus example. The .sqlite file is gitignored; this script is
# the source of truth for the database contents.
#
# Requires `sqlite3` on PATH.

set -e

cd "$(dirname "$0")"
rm -f shifts.sqlite

sqlite3 shifts.sqlite <<'SQL'
CREATE TABLE morning (
    id   INTEGER NOT NULL,
    name TEXT NOT NULL,

    PRIMARY KEY (id)
);

CREATE TABLE evening (
    id   INTEGER NOT NULL,
    name TEXT NOT NULL,

    PRIMARY KEY (id)
);

INSERT INTO morning (id, name) VALUES
    (1, 'Ada'),
    (2, 'Grace'),
    (3, 'Alan')
;

INSERT INTO evening (id, name) VALUES
    (2, 'Grace'),
    (3, 'Alan'),
    (4, 'Edsger')
;
SQL
