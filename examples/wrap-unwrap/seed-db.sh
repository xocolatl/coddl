#!/usr/bin/env sh
# examples/wrap-unwrap/seed-db.sh
#
# Recreates points.sqlite with the demo data shared with the in-process
# wrap/unwrap example. The .sqlite file is gitignored; this script is the
# source of truth for the database contents.
#
# Requires `sqlite3` on PATH.

set -e

cd "$(dirname "$0")"
rm -f points.sqlite

sqlite3 points.sqlite <<'SQL'
CREATE TABLE points (
    id INTEGER NOT NULL,
    x  INTEGER NOT NULL,
    y  INTEGER NOT NULL,

    PRIMARY KEY (id)
);

INSERT INTO points (id, x, y) VALUES
    (1, 10, 20),
    (2, 30, 40)
;
SQL
