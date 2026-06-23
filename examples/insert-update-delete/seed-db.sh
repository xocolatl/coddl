#!/usr/bin/env sh
# examples/insert-update-delete/seed-db.sh
#
# Recreates greetings.sqlite with a few rows for the write demo. The .sqlite
# file is gitignored; this script is the source of truth for its contents.
#
# Requires `sqlite3` on PATH.

set -e

cd "$(dirname "$0")"
rm -f greetings.sqlite

sqlite3 greetings.sqlite <<'SQL'
CREATE TABLE greetings (
    id      INTEGER NOT NULL,
    message TEXT NOT NULL,
    PRIMARY KEY (id)
);

CREATE TABLE stale (
    id      INTEGER NOT NULL,
    message TEXT NOT NULL,
    PRIMARY KEY (id)
);

CREATE TABLE new_arrivals (
    id      INTEGER NOT NULL,
    message TEXT NOT NULL,
    PRIMARY KEY (id)
);

INSERT INTO greetings (id, message) VALUES
    (1, 'hello world'),
    (2, 'goodbye'),
    (3, 'farewell'),
    (4, 'so long');

-- Exact tuples to purge from greetings (matched on id AND message).
INSERT INTO stale (id, message) VALUES
    (2, 'goodbye'),
    (3, 'farewell');

-- Tuples to add. (4, 'so long') is already present, so the union is a no-op
-- for it (idempotent); (5, 'howdy') is genuinely new.
INSERT INTO new_arrivals (id, message) VALUES
    (4, 'so long'),
    (5, 'howdy');
SQL

echo "seeded greetings.sqlite"
