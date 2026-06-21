#!/usr/bin/env sh
# examples/transitive-closure/seed-db.sh
#
# Recreates tclose.sqlite with the demo data shared with the in-process
# transitive-closure example. The .sqlite file is gitignored; this script is
# the source of truth for the database contents.
#
# `from`/`to` are reserved SQL keywords used as column names, so they are
# quoted in the DDL — mirroring how the Coddl emitter quotes every identifier.
#
# Requires `sqlite3` on PATH.

set -e

cd "$(dirname "$0")"
rm -f tclose.sqlite

sqlite3 tclose.sqlite <<'SQL'
CREATE TABLE edges (
    "from" INTEGER NOT NULL,
    "to"   INTEGER NOT NULL,

    PRIMARY KEY ("from", "to")
);

CREATE TABLE contains (
    major INTEGER NOT NULL,
    minor INTEGER NOT NULL,
    qty   INTEGER NOT NULL,

    PRIMARY KEY (major, minor)
);

INSERT INTO edges ("from", "to") VALUES
    (1, 2),
    (2, 3),
    (3, 4)
;

INSERT INTO contains (major, minor, qty) VALUES
    (1, 2, 2),
    (1, 3, 1),
    (2, 4, 32),
    (3, 5, 1)
;
SQL
