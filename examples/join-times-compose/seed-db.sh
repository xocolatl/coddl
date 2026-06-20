#!/usr/bin/env sh
# examples/join-times-compose/seed-db.sh
#
# Recreates staffing.sqlite with the demo data shared with the in-process
# join-times-compose example. The .sqlite file is gitignored; this script is
# the source of truth for the database contents.
#
# Requires `sqlite3` on PATH.

set -e

cd "$(dirname "$0")"
rm -f staffing.sqlite

sqlite3 staffing.sqlite <<'SQL'
CREATE TABLE employees (
    emp_id   INTEGER NOT NULL,
    emp_name TEXT NOT NULL,
    dept_id  INTEGER NOT NULL,

    PRIMARY KEY (emp_id)
);

CREATE TABLE departments (
    dept_id   INTEGER NOT NULL,
    dept_name TEXT NOT NULL,

    PRIMARY KEY (dept_id)
);

CREATE TABLE job_titles (
    title TEXT NOT NULL,

    PRIMARY KEY (title)
);

CREATE TABLE locations (
    location TEXT NOT NULL,

    PRIMARY KEY (location)
);

INSERT INTO employees (emp_id, emp_name, dept_id) VALUES
    (1, 'Ada',    10),
    (2, 'Grace',  10),
    (3, 'Alan',   20),
    (4, 'Edsger', 30)
;

INSERT INTO departments (dept_id, dept_name) VALUES
    (10, 'Engineering'),
    (20, 'Sales'),
    (30, 'Marketing')
;

INSERT INTO job_titles (title) VALUES
    ('Engineer'),
    ('Manager')
;

INSERT INTO locations (location) VALUES
    ('London'),
    ('Paris')
;
SQL
