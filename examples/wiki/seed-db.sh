#!/usr/bin/env sh
# examples/wiki/seed-db.sh
#
# Recreates wiki.sqlite with the demo pages for the A0 web handler. The .sqlite
# file is gitignored; this script is the source of truth for the database
# contents. Every column is TEXT (slug is the primary key).
#
# Requires `sqlite3` on PATH.

set -e

cd "$(dirname "$0")"
rm -f wiki.sqlite

sqlite3 wiki.sqlite <<'SQL'
CREATE TABLE pages (
    slug  TEXT NOT NULL,
    title TEXT NOT NULL,
    body  TEXT NOT NULL,

    PRIMARY KEY (slug)
);

INSERT INTO pages (slug, title, body) VALUES
    ('home',  'Home',  'Welcome to the wiki.'),
    ('about', 'About', 'This wiki is built in Coddl.')
;
SQL
