#!/usr/bin/env sh
# examples/extend-replace/seed-db.sh
#
# Recreates sales.sqlite with the demo data shared with the in-process
# extend/replace example. The .sqlite file is gitignored; this script is the
# source of truth for the database contents.
#
# Requires `sqlite3` on PATH.

set -e

cd "$(dirname "$0")"
rm -f sales.sqlite

sqlite3 sales.sqlite <<'SQL'
CREATE TABLE sales (
    id         INTEGER NOT NULL,
    customer   TEXT NOT NULL,
    item       TEXT NOT NULL,
    unit_cents INTEGER NOT NULL,
    qty        INTEGER NOT NULL,

    PRIMARY KEY (id)
);

INSERT INTO sales (id, customer, item, unit_cents, qty) VALUES
    (1, 'ada', 'widget', 500, 3),
    (2, 'bo',  'gadget', 800, 2)
;
SQL
