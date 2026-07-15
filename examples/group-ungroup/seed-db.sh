#!/usr/bin/env sh
# examples/group-ungroup/seed-db.sh
#
# Recreates shipments.sqlite with the demo data shared with the in-process
# group/ungroup example. The .sqlite file is gitignored; this script is the
# source of truth for the database contents.
#
# Requires `sqlite3` on PATH.

set -e

cd "$(dirname "$0")"
rm -f shipments.sqlite

sqlite3 shipments.sqlite <<'SQL'
CREATE TABLE shipments (
    supplier TEXT    NOT NULL,
    part     TEXT    NOT NULL,
    qty      INTEGER NOT NULL,

    PRIMARY KEY (supplier, part)
);

INSERT INTO shipments (supplier, part, qty) VALUES
    ('S1', 'P1', 300),
    ('S1', 'P2', 200),
    ('S2', 'P1', 100)
;
SQL
