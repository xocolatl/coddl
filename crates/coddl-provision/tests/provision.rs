//! End-to-end tests for `provision_catalog`: self-authored `.cddb` + `.cdstore`
//! fixtures in a temp dir, driven through the fold and the SQLite executor, with
//! the resulting database reopened to check what actually landed. No fixture is
//! read from `examples/` — every test authors its own catalog.

use std::path::{Path, PathBuf};

use coddl_provision::provision_catalog;
use rusqlite::Connection;

/// Write `<db>.cddb` and `<db>.cdstore` into `dir`; return the `.cddb` path.
fn write_catalog(dir: &Path, db: &str, cddb: &str, cdstore: &str) -> PathBuf {
    let cddb_path = dir.join(format!("{db}.cddb"));
    std::fs::write(&cddb_path, cddb).unwrap();
    std::fs::write(dir.join(format!("{db}.cdstore")), cdstore).unwrap();
    cddb_path
}

/// A minimal `.cdstore` binding each relvar to a same-named lowercase table with
/// bare-name columns; the SQLite file lands beside the `.cdstore` (the temp dir).
fn cdstore(db: &str, relvars: &[(&str, &str, &[&str])]) -> String {
    let mut s =
        format!("store for {db};\n\nbackend sqlite {{\n    file: \"{db}.sqlite\",\n}};\n\n");
    for (name, table, cols) in relvars {
        s.push_str(&format!(
            "relvar {name}: table \"{table}\" {{\n    columns: {{ {} }}\n}};\n\n",
            cols.join(", ")
        ));
    }
    s
}

fn error_codes(diags: &[coddl_diagnostics::Diagnostic]) -> Vec<&str> {
    diags
        .iter()
        .filter(|d| d.severity == coddl_diagnostics::Severity::Error)
        .map(|d| d.code)
        .collect()
}

// coddl-diagnostics is a transitive dep; name it for `error_codes`.
extern crate coddl_diagnostics;

#[test]
fn seeds_a_suppliers_shaped_catalog_and_reopens_it() {
    let dir = tempfile::tempdir().unwrap();
    let cddb = r#"
database sphappy;

base relvar S { sno: Text, status: Integer, } key { sno };
S := Relation {
    { sno: "S1", status: 20 },
    { sno: "S2", status: 10 },
    { sno: "S3", status: 30 },
};

base relvar P { pno: Text, weight: Rational, } key { pno };
P := Relation {
    { pno: "P1", weight: 12.0 },
    { pno: "P2", weight: 17.0 },
};

base relvar SP { sno: Text, pno: Text, qty: Integer, } key { sno, pno };
SP := Relation {
    { sno: "S1", pno: "P1", qty: 300 },
    { sno: "S1", pno: "P2", qty: 200 },
};
"#;
    let store = cdstore(
        "sphappy",
        &[
            ("S", "s", &["sno", "status"]),
            ("P", "p", &["pno", "weight"]),
            ("SP", "sp", &["sno", "pno", "qty"]),
        ],
    );
    let cddb_path = write_catalog(dir.path(), "sphappy", cddb, &store);

    let out = provision_catalog(&cddb_path);
    assert!(
        error_codes(&out.diagnostics).is_empty(),
        "unexpected diagnostics: {:?}",
        out.diagnostics
    );
    let report = out.report.expect("a report on success");
    // Every table is freshly created; counts match the (deduped) INIT.
    assert!(report.tables.iter().all(|t| t.created));
    let counts: std::collections::HashMap<&str, usize> = report
        .tables
        .iter()
        .map(|t| (t.table.as_str(), t.rows_inserted))
        .collect();
    assert_eq!(counts["s"], 3);
    assert_eq!(counts["p"], 2);
    assert_eq!(counts["sp"], 2);

    // Reopen the real database: the Rational weight is stored as canonical TEXT.
    let conn = Connection::open(dir.path().join("sphappy.sqlite")).unwrap();
    let weight: String = conn
        .query_row("SELECT weight FROM p WHERE pno = 'P1'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(weight, "12/1");
    let status: i64 = conn
        .query_row("SELECT status FROM s WHERE sno = 'S2'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(status, 10);
}

#[test]
fn evaluates_constant_init_expressions() {
    let dir = tempfile::tempdir().unwrap();
    // `E`: Integer arithmetic (`6 * 6`), unary minus on Integer (`-5`), and an
    // Integer literal widened into a Rational column (`r`). Each column's
    // literal kind is uniform across tuples (a relation literal's tuples must
    // share a heading), so `r` is Integer in every tuple and widens at fold
    // time. `F`: unary minus and a plain literal in a Rational column.
    let cddb = r#"
database speval;

base relvar E { id: Text, n: Integer, r: Rational, } key { id };
E := Relation {
    { id: "a", n: 6 * 6, r: 3 },
    { id: "b", n: -5, r: 10 },
};

base relvar F { id: Text, w: Rational, } key { id };
F := Relation {
    { id: "a", w: -12.0 },
    { id: "b", w: 17.0 },
};
"#;
    let store = cdstore(
        "speval",
        &[("E", "e", &["id", "n", "r"]), ("F", "f", &["id", "w"])],
    );
    let cddb_path = write_catalog(dir.path(), "speval", cddb, &store);

    let out = provision_catalog(&cddb_path);
    assert!(
        error_codes(&out.diagnostics).is_empty(),
        "unexpected diagnostics: {:?}",
        out.diagnostics
    );

    let conn = Connection::open(dir.path().join("speval.sqlite")).unwrap();
    let (n_a, r_a): (i64, String) = conn
        .query_row("SELECT n, r FROM e WHERE id = 'a'", [], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .unwrap();
    assert_eq!(n_a, 36); // 6 * 6
    assert_eq!(r_a, "3/1"); // Integer 3 widened into the Rational column
    let n_b: i64 = conn
        .query_row("SELECT n FROM e WHERE id = 'b'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n_b, -5); // unary minus on an Integer

    // Unary minus and a bare literal in a Rational column.
    let w_a: String = conn
        .query_row("SELECT w FROM f WHERE id = 'a'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(w_a, "-12/1");
    let w_b: String = conn
        .query_row("SELECT w FROM f WHERE id = 'b'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(w_b, "17/1");
}

#[test]
fn exact_duplicate_tuples_coalesce() {
    let dir = tempfile::tempdir().unwrap();
    let cddb = r#"
database spdup;

base relvar T { k: Text, v: Integer, } key { k };
T := Relation {
    { k: "x", v: 1 },
    { k: "x", v: 1 },
    { k: "y", v: 2 },
};
"#;
    let store = cdstore("spdup", &[("T", "t", &["k", "v"])]);
    let cddb_path = write_catalog(dir.path(), "spdup", cddb, &store);

    let out = provision_catalog(&cddb_path);
    assert!(error_codes(&out.diagnostics).is_empty());
    let report = out.report.expect("success");
    // The identical `{x,1}` tuple is coalesced (a relation is a set): 2 rows.
    assert_eq!(report.tables[0].rows_inserted, 2);
}

#[test]
fn key_collision_errors_and_never_touches_the_database() {
    let dir = tempfile::tempdir().unwrap();
    let cddb = r#"
database spcol;

base relvar T { k: Text, v: Integer, } key { k };
T := Relation {
    { k: "x", v: 1 },
    { k: "x", v: 2 },
};
"#;
    let store = cdstore("spcol", &[("T", "t", &["k", "v"])]);
    let cddb_path = write_catalog(dir.path(), "spcol", cddb, &store);

    let out = provision_catalog(&cddb_path);
    assert!(out.report.is_none());
    assert!(error_codes(&out.diagnostics).contains(&"PV0005"));
    // Fold-time validation is pre-SQL: the database file was never created.
    assert!(!dir.path().join("spcol.sqlite").exists());
}

#[test]
fn unprovisionable_column_type_errors() {
    let dir = tempfile::tempdir().unwrap();
    // A `Binary` attribute has no seedable column kind (no literal form).
    let cddb = r#"
database spbin;

base relvar B { id: Text, blob: Binary, } key { id };
"#;
    let store = cdstore("spbin", &[("B", "b", &["id", "blob"])]);
    let cddb_path = write_catalog(dir.path(), "spbin", cddb, &store);

    let out = provision_catalog(&cddb_path);
    assert!(out.report.is_none());
    assert!(error_codes(&out.diagnostics).contains(&"PV0001"));
    assert!(!dir.path().join("spbin.sqlite").exists());
}

#[test]
fn schema_mismatch_rolls_back_byte_identical() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("spmis.sqlite");
    // Pre-create a table that does NOT match the catalog (an extra column).
    {
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch("CREATE TABLE t (k TEXT, v INTEGER, extra TEXT);")
            .unwrap();
    }
    let before = std::fs::read(&db_path).unwrap();

    let cddb = r#"
database spmis;

base relvar T { k: Text, v: Integer, } key { k };
T := Relation { { k: "x", v: 1 }, };
"#;
    let store = cdstore("spmis", &[("T", "t", &["k", "v"])]);
    let cddb_path = write_catalog(dir.path(), "spmis", cddb, &store);

    let out = provision_catalog(&cddb_path);
    assert!(out.report.is_none());
    assert!(error_codes(&out.diagnostics).contains(&"PV0008"));
    // Rollback leaves the database byte-for-byte unchanged.
    assert_eq!(std::fs::read(&db_path).unwrap(), before);
}

#[test]
fn view_named_like_a_table_errors() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("spview.sqlite");
    {
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch("CREATE VIEW t AS SELECT 1 AS k;")
            .unwrap();
    }
    let cddb = r#"
database spview;

base relvar T { k: Integer, } key { k };
T := Relation { { k: 1 }, };
"#;
    let store = cdstore("spview", &[("T", "t", &["k"])]);
    let cddb_path = write_catalog(dir.path(), "spview", cddb, &store);

    let out = provision_catalog(&cddb_path);
    assert!(out.report.is_none());
    assert!(error_codes(&out.diagnostics).contains(&"PV0009"));
}

#[test]
fn reprovision_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let cddb = r#"
database spidem;

base relvar T { k: Text, v: Integer, } key { k };
T := Relation { { k: "x", v: 1 }, { k: "y", v: 2 }, };
"#;
    let store = cdstore("spidem", &[("T", "t", &["k", "v"])]);
    let cddb_path = write_catalog(dir.path(), "spidem", cddb, &store);

    let first = provision_catalog(&cddb_path);
    assert!(error_codes(&first.diagnostics).is_empty());
    assert!(first.report.unwrap().tables[0].created);

    let second = provision_catalog(&cddb_path);
    assert!(error_codes(&second.diagnostics).is_empty());
    let report = second.report.unwrap();
    // Second run verifies the existing table (no CREATE) and re-seeds the rows.
    assert!(!report.tables[0].created);
    assert_eq!(report.tables[0].rows_inserted, 2);
}

#[test]
fn env_override_selects_the_target_file() {
    let dir = tempfile::tempdir().unwrap();
    let override_path = dir.path().join("elsewhere.sqlite");
    // Unique db name → unique env key → no cross-test interference.
    std::env::set_var("CODDL_SPENV_FILE", &override_path);

    let cddb = r#"
database spenv;

base relvar T { k: Text, } key { k };
T := Relation { { k: "x" }, };
"#;
    let store = cdstore("spenv", &[("T", "t", &["k"])]);
    let cddb_path = write_catalog(dir.path(), "spenv", cddb, &store);

    let out = provision_catalog(&cddb_path);
    std::env::remove_var("CODDL_SPENV_FILE");

    assert!(
        error_codes(&out.diagnostics).is_empty(),
        "{:?}",
        out.diagnostics
    );
    // The env-named file was seeded; the baked default path was not created.
    assert!(override_path.exists());
    assert!(!dir.path().join("spenv.sqlite").exists());
}
