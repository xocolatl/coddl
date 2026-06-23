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

INSERT INTO greetings (id, message) VALUES
    (1, 'hello world'),
    (2, 'goodbye'),
    (3, 'farewell');
SQL

echo "seeded greetings.sqlite"
