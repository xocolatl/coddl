//! Per-document analysis layer.
//!
//! Owns the LSP-visible document store. Each `Document` lazily
//! computes a `Snapshot` (parsed tree + diagnostics + hints + line
//! index) on demand and caches it by version. The first request at
//! a given version pays the cost; later requests at the same
//! version return the cached `Arc<Snapshot>`. `did_change`
//! invalidates by bumping the version.
//!
//! CPU work runs on `tokio::task::spawn_blocking` so the LSP IO
//! loop is never blocked by analysis. The `inner` mutex is a
//! `tokio::sync::Mutex` so it can be held across `await` points.
//!
//! This is the seam every future LSP feature plugs into. Hover,
//! go-to-def, completion, semantic tokens — all of them become
//! `analyzer.snapshot(uri).map(...)`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use coddl_diagnostics::{Diagnostic, FileId};
use coddl_syntax::ast::{AstNode, Root};
use coddl_syntax::FileKind;
use coddl_types::TypeHint;
use tokio::sync::{Mutex, RwLock};
use tower_lsp::lsp_types::Url;

use crate::line_index::LineIndex;

/// FileIds used by the project plan layer. These match the
/// constants `coddl_plan` emits in its diagnostics: per-document
/// snapshots still pass `FileId(0)` to the typechecker, but plan
/// diagnostics are tagged with these so the analyzer can route
/// them back to the right buffer.
const PLAN_FILE_ID_CD: FileId = FileId(0);
const PLAN_FILE_ID_CDDB: FileId = FileId(1);
const PLAN_FILE_ID_CDSTORE: FileId = FileId(2);

/// Derive the dialect for a document from its URI's path extension.
/// Unrecognized extensions default to [`FileKind::Cd`] so the rest of
/// the pipeline stays uniform.
fn kind_from_uri(uri: &Url) -> FileKind {
    FileKind::from_path(Path::new(uri.path())).unwrap_or(FileKind::Cd)
}

pub struct Analyzer {
    files: RwLock<HashMap<Url, Arc<Document>>>,
    /// Projects keyed on the `.cd` entry point's canonical
    /// filesystem path. Multiple project members (the `.cd` and any
    /// open companions) share the same project entry; the cross-
    /// file plan snapshot is cached here.
    projects: RwLock<HashMap<PathBuf, Arc<Project>>>,
}

impl Default for Analyzer {
    fn default() -> Self {
        Self::new()
    }
}

impl Analyzer {
    pub fn new() -> Self {
        Self {
            files: RwLock::new(HashMap::new()),
            projects: RwLock::new(HashMap::new()),
        }
    }

    /// Insert or replace the document at `uri`. Invalidates any
    /// cached snapshot — the next `snapshot()` recomputes. The
    /// document's [`FileKind`] is resolved from `uri`'s extension on
    /// first insert and stays fixed for the document's lifetime.
    ///
    /// Also runs project discovery / membership maintenance: a `.cd`
    /// open registers a project; a `.cddb` / `.cdstore` open binds
    /// itself to whatever project's `.cd` declares a matching
    /// `database <name>;`. Edits to any project member invalidate
    /// the cached project snapshot.
    pub async fn put_document(&self, uri: Url, version: i32, source: String) {
        let kind = kind_from_uri(&uri);
        let source: Arc<str> = Arc::from(source);
        let mut files = self.files.write().await;
        match files.get(&uri) {
            Some(doc) => {
                let mut inner = doc.inner.lock().await;
                inner.version = version;
                inner.source = source;
                inner.snapshot = None;
            }
            None => {
                files.insert(
                    uri.clone(),
                    Arc::new(Document {
                        kind,
                        inner: Mutex::new(DocumentInner {
                            version,
                            source,
                            snapshot: None,
                            project: None,
                        }),
                    }),
                );
            }
        }
        drop(files);

        // Run project discovery / membership maintenance for this
        // URI. Project edits invalidate the project snapshot so the
        // next request recomputes against the just-updated source.
        let _ = self.discover_and_bind_project(&uri, kind).await;
        self.invalidate_project_for_uri(&uri).await;
    }

    pub async fn close_document(&self, uri: &Url) {
        let project_id = {
            let files = self.files.read().await;
            if let Some(doc) = files.get(uri) {
                let inner = doc.inner.lock().await;
                inner.project.clone()
            } else {
                None
            }
        };

        {
            let mut files = self.files.write().await;
            files.remove(uri);
        }

        if let Some(cd_path) = project_id {
            self.unbind_uri_from_project(&cd_path, uri).await;
        }
    }

    /// Resolve the analyzed snapshot for `uri`. Computes lazily on
    /// miss; reuses the cached snapshot when its version matches the
    /// document's current version.
    ///
    /// For `.cd` documents, runs the full `coddl_types::check`
    /// pipeline (parse + typecheck + hints). For dialect documents
    /// (`.cddb` / `.cdmap` / `.cdstore`), runs the parser only — the
    /// typecheck pass for the new dialects lands in later phases.
    pub async fn snapshot(&self, uri: &Url) -> Option<Arc<Snapshot>> {
        let doc = {
            let files = self.files.read().await;
            files.get(uri).cloned()?
        };
        let kind = doc.kind;

        let (source, version) = {
            let inner = doc.inner.lock().await;
            if let Some(snap) = &inner.snapshot {
                if snap.version == inner.version {
                    return Some(snap.clone());
                }
            }
            (inner.source.clone(), inner.version)
        };

        // Compute off the IO loop. The `Arc<str>` keeps the source
        // alive for the `LineIndex` to share. The parse / check
        // result's `tree` field carries a `SyntaxNode` that's `!Sync`
        // (rowan uses an `Rc` internally for the cursor view), so we
        // decompose the result here and store only the Send/Sync
        // pieces. When a future feature needs the parsed tree
        // (semantic tokens, hover), the right move is to also store
        // the underlying `GreenNode` (which *is* Sync) and
        // reconstitute the `SyntaxNode` per request.
        let source_for_blocking = source.clone();
        let snap_arc = tokio::task::spawn_blocking(move || {
            let (diagnostics, hints) = match kind {
                FileKind::Cd | FileKind::Cddb => {
                    let check = coddl_types::check(&source_for_blocking, FileId(0), kind);
                    (check.diagnostics, check.hints)
                }
                other => {
                    let parse_out = coddl_syntax::parse(&source_for_blocking, FileId(0), other);
                    (parse_out.diagnostics, Vec::new())
                }
            };
            let line_index = LineIndex::new(source_for_blocking.clone());
            Arc::new(Snapshot {
                source: source_for_blocking,
                diagnostics,
                hints,
                line_index,
                version,
            })
        })
        .await
        .ok()?;

        // Cache only if the document hasn't moved on under us. A
        // `did_change` between the spawn and the await would have
        // bumped `inner.version`; we still return our freshly
        // computed snapshot to the caller, but don't cache it.
        {
            let mut inner = doc.inner.lock().await;
            if inner.version == snap_arc.version {
                inner.snapshot = Some(snap_arc.clone());
            }
        }
        Some(snap_arc)
    }
}

// ── Project discovery + recompute ────────────────────────────────────

impl Analyzer {
    /// Return the `FileId` `uri` plays in `project`, if any.
    pub async fn file_id_for(&self, project: &Arc<Project>, uri: &Url) -> Option<FileId> {
        let inner = project.inner.lock().await;
        inner
            .members
            .iter()
            .find(|(_, u)| *u == uri)
            .map(|(fid, _)| *fid)
    }

    /// Return every URI currently bound to `project`.
    pub async fn project_members(&self, project: &Arc<Project>) -> Vec<Url> {
        let inner = project.inner.lock().await;
        inner.members.values().cloned().collect()
    }

    /// Look up the project this `uri` participates in, if any.
    pub async fn project_for(&self, uri: &Url) -> Option<Arc<Project>> {
        let files = self.files.read().await;
        let doc = files.get(uri)?.clone();
        drop(files);
        let cd_path = {
            let inner = doc.inner.lock().await;
            inner.project.clone()?
        };
        let projects = self.projects.read().await;
        projects.get(&cd_path).cloned()
    }

    /// Compute (or reuse) the cross-file plan snapshot for the
    /// project identified by `cd_path`. The recompute runs on
    /// `spawn_blocking`; the snapshot caches until any project
    /// member edits.
    pub async fn project_snapshot(&self, cd_path: &Path) -> Option<Arc<ProjectSnapshot>> {
        let project = {
            let projects = self.projects.read().await;
            projects.get(cd_path).cloned()?
        };

        // Fast path: cached snapshot.
        {
            let inner = project.inner.lock().await;
            if let Some(snap) = &inner.snapshot {
                return Some(snap.clone());
            }
        }

        // Gather current open-buffer overrides for every member.
        let overrides = self.build_plan_overrides(&project).await;
        let cd_path_for_blocking = cd_path.to_path_buf();
        let overrides_for_blocking = overrides.clone();

        let snap_arc = tokio::task::spawn_blocking(move || {
            let out = coddl_plan::discover_and_validate_with_overrides(
                &cd_path_for_blocking,
                &overrides_for_blocking,
            );
            build_project_snapshot(out.diagnostics, overrides_for_blocking, &cd_path_for_blocking)
        })
        .await
        .ok()?;

        let snap_arc = Arc::new(snap_arc);
        let mut inner = project.inner.lock().await;
        inner.snapshot = Some(snap_arc.clone());
        Some(snap_arc)
    }

    /// Discover a project for `uri` and add `uri` to its members.
    /// For a `.cd` open this registers the project; for a `.cddb` /
    /// `.cdstore` open this attaches to (or creates) the project
    /// whose `.cd` declares a matching `database <name>;` binding.
    async fn discover_and_bind_project(&self, uri: &Url, kind: FileKind) {
        let Some(uri_path) = uri_to_path(uri) else {
            return;
        };

        match kind {
            FileKind::Cd => {
                // The project id is this .cd's path. Parse the
                // buffer source to extract the database binding.
                let source = self.get_source(uri).await;
                let database_name = source
                    .as_deref()
                    .and_then(extract_database_binding_name);

                let project = self
                    .get_or_create_project(uri_path.clone(), database_name.clone())
                    .await;

                // Bind .cd → uri.
                {
                    let mut inner = project.inner.lock().await;
                    inner.members.insert(PLAN_FILE_ID_CD, uri.clone());
                    inner.database_name = database_name.clone();
                    inner.snapshot = None;
                }
                self.set_doc_project(uri, &uri_path).await;

                // Sweep already-open .cddb / .cdstore docs in the
                // same directory whose names match the binding and
                // bind them too.
                if let Some(name) = database_name {
                    self.attach_open_companions(&project, &uri_path, &name)
                        .await;
                }
            }
            FileKind::Cddb | FileKind::Cdstore => {
                // The companion's basename minus extension is the
                // database name we need to match against some .cd's
                // binding. Look for a project whose database_name
                // matches; if none, scan the directory for .cd files
                // whose binding matches and create a project for
                // the first match.
                let Some(db_name) = file_stem(&uri_path) else {
                    return;
                };
                let Some(cd_path) = self
                    .find_cd_for_database(&uri_path, &db_name)
                    .await
                else {
                    return;
                };

                let project = self
                    .get_or_create_project(cd_path.clone(), Some(db_name))
                    .await;
                let fid = if kind == FileKind::Cddb {
                    PLAN_FILE_ID_CDDB
                } else {
                    PLAN_FILE_ID_CDSTORE
                };
                {
                    let mut inner = project.inner.lock().await;
                    inner.members.insert(fid, uri.clone());
                    inner.snapshot = None;
                }
                self.set_doc_project(uri, &cd_path).await;
            }
            _ => {} // .cdmap: not part of v1 project model
        }
    }

    /// Invalidate the project snapshot the URI belongs to, if any.
    /// Called after every `put_document` so subsequent
    /// `project_snapshot` calls recompute against the latest source.
    async fn invalidate_project_for_uri(&self, uri: &Url) {
        let project_id = {
            let files = self.files.read().await;
            let Some(doc) = files.get(uri) else { return };
            let inner = doc.inner.lock().await;
            inner.project.clone()
        };
        if let Some(cd_path) = project_id {
            let projects = self.projects.read().await;
            if let Some(project) = projects.get(&cd_path) {
                let mut inner = project.inner.lock().await;
                inner.snapshot = None;
            }
        }
    }

    /// Remove a URI from a project's member list. If no members
    /// remain, drop the project entry.
    async fn unbind_uri_from_project(&self, cd_path: &Path, uri: &Url) {
        let mut projects = self.projects.write().await;
        let Some(project) = projects.get(cd_path).cloned() else {
            return;
        };
        let empty = {
            let mut inner = project.inner.lock().await;
            inner.members.retain(|_, u| u != uri);
            inner.snapshot = None;
            inner.members.is_empty()
        };
        if empty {
            projects.remove(cd_path);
        }
    }

    async fn get_or_create_project(
        &self,
        cd_path: PathBuf,
        database_name: Option<String>,
    ) -> Arc<Project> {
        let mut projects = self.projects.write().await;
        if let Some(p) = projects.get(&cd_path) {
            return p.clone();
        }
        let project = Arc::new(Project {
            cd_path: cd_path.clone(),
            inner: Mutex::new(ProjectInner {
                database_name,
                members: HashMap::new(),
                snapshot: None,
            }),
        });
        projects.insert(cd_path, project.clone());
        project
    }

    async fn set_doc_project(&self, uri: &Url, cd_path: &Path) {
        let files = self.files.read().await;
        if let Some(doc) = files.get(uri) {
            let mut inner = doc.inner.lock().await;
            inner.project = Some(cd_path.to_path_buf());
        }
    }

    async fn get_source(&self, uri: &Url) -> Option<Arc<str>> {
        let files = self.files.read().await;
        let doc = files.get(uri)?;
        let inner = doc.inner.lock().await;
        Some(inner.source.clone())
    }

    /// Sweep open .cddb / .cdstore docs whose paths match
    /// `<dir>/<database_name>.<ext>` and bind them to `project`.
    async fn attach_open_companions(&self, project: &Arc<Project>, cd_path: &Path, db_name: &str) {
        let Some(dir) = cd_path.parent() else { return };
        let candidates = [
            (PLAN_FILE_ID_CDDB, dir.join(format!("{db_name}.cddb"))),
            (PLAN_FILE_ID_CDSTORE, dir.join(format!("{db_name}.cdstore"))),
        ];
        let files = self.files.read().await;
        for (fid, expected_path) in &candidates {
            for (uri, doc) in files.iter() {
                if uri_to_path(uri).as_deref() != Some(expected_path) {
                    continue;
                }
                {
                    let mut p_inner = project.inner.lock().await;
                    p_inner.members.insert(*fid, uri.clone());
                }
                let mut doc_inner = doc.inner.lock().await;
                doc_inner.project = Some(project.cd_path.clone());
            }
        }
    }

    /// Reverse-lookup: given a companion file's path, find a `.cd`
    /// in the same directory whose `database <name>;` matches.
    /// First checks open .cd documents (their buffer source is
    /// authoritative), then falls back to scanning the directory.
    async fn find_cd_for_database(&self, companion_path: &Path, db_name: &str) -> Option<PathBuf> {
        let dir = companion_path.parent()?;

        // 1. Already-discovered project keyed on `database_name`.
        {
            let projects = self.projects.read().await;
            for (cd_path, project) in projects.iter() {
                if cd_path.parent() != Some(dir) {
                    continue;
                }
                let inner = project.inner.lock().await;
                if inner.database_name.as_deref() == Some(db_name) {
                    return Some(cd_path.clone());
                }
            }
        }

        // 2. Open .cd documents in the same directory.
        {
            let files = self.files.read().await;
            for (uri, doc) in files.iter() {
                if doc.kind != FileKind::Cd {
                    continue;
                }
                let Some(path) = uri_to_path(uri) else { continue };
                if path.parent() != Some(dir) {
                    continue;
                }
                let source = {
                    let inner = doc.inner.lock().await;
                    inner.source.clone()
                };
                if extract_database_binding_name(&source).as_deref() == Some(db_name) {
                    return Some(path);
                }
            }
        }

        // 3. Disk scan for .cd files with a matching binding.
        let entries = std::fs::read_dir(dir).ok()?;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("cd") {
                continue;
            }
            let Ok(source) = std::fs::read_to_string(&path) else {
                continue;
            };
            if extract_database_binding_name(&source).as_deref() == Some(db_name) {
                return Some(path);
            }
        }
        None
    }

    /// Build the `overrides` map handed to the plan layer: every
    /// open project member's current buffer source is fed in by
    /// path; closed members fall back to disk reads inside the
    /// plan layer itself.
    async fn build_plan_overrides(&self, project: &Arc<Project>) -> HashMap<PathBuf, String> {
        let mut overrides: HashMap<PathBuf, String> = HashMap::new();
        let members = {
            let inner = project.inner.lock().await;
            inner.members.clone()
        };
        let files = self.files.read().await;
        for uri in members.values() {
            let Some(path) = uri_to_path(uri) else { continue };
            let Some(doc) = files.get(uri) else { continue };
            let source = {
                let inner = doc.inner.lock().await;
                inner.source.clone()
            };
            overrides.insert(path, source.to_string());
        }
        overrides
    }
}

// ── Free helpers ─────────────────────────────────────────────────────

fn uri_to_path(uri: &Url) -> Option<PathBuf> {
    uri.to_file_path().ok()
}

fn file_stem(path: &Path) -> Option<String> {
    path.file_stem()
        .and_then(|s| s.to_str())
        .map(|s| s.to_string())
}

/// Parse the `.cd` source enough to extract its `database <name>;`
/// binding. Cheap — runs the parser but bails the moment a
/// `DATABASE_BINDING` item is found.
fn extract_database_binding_name(source: &str) -> Option<String> {
    let parse_out = coddl_syntax::parse(source, FileId(0), FileKind::Cd);
    let root = Root::cast(parse_out.tree)?;
    for item in root.items() {
        if let coddl_syntax::ast::Item::DatabaseBinding(b) = item {
            return b.name().map(|t| t.text().to_string());
        }
    }
    None
}

/// Construct a `ProjectSnapshot` from the plan diagnostics: group
/// by `FileId` and build a `LineIndex` per file from the source the
/// plan layer saw (the override map or disk fallback).
fn build_project_snapshot(
    diagnostics: Vec<Diagnostic>,
    overrides: HashMap<PathBuf, String>,
    cd_path: &Path,
) -> ProjectSnapshot {
    let mut diagnostics_by_file: HashMap<FileId, Vec<Diagnostic>> = HashMap::new();
    for d in diagnostics {
        diagnostics_by_file.entry(d.span.file).or_default().push(d);
    }

    let dir = cd_path.parent();
    let parent = dir.map(|d| d.to_path_buf());
    let read_source = |path: &Path| -> Option<String> {
        if let Some(s) = overrides.get(path) {
            return Some(s.clone());
        }
        std::fs::read_to_string(path).ok()
    };

    let mut line_indices: HashMap<FileId, LineIndex> = HashMap::new();
    if let Some(s) = read_source(cd_path) {
        line_indices.insert(PLAN_FILE_ID_CD, LineIndex::new(Arc::from(s)));
    }
    if let Some(p) = parent.as_ref() {
        // Match the path the plan layer constructs: dir/<db>.cddb /
        // dir/<db>.cdstore. We don't have the db name here, but
        // there's at most one of each in the directory we care
        // about — read whichever paths show up in overrides + the
        // canonical names derived from open file paths.
        for (path, src) in &overrides {
            if path == cd_path {
                continue;
            }
            if path.parent() != Some(p) {
                continue;
            }
            let fid = match path.extension().and_then(|s| s.to_str()) {
                Some("cddb") => PLAN_FILE_ID_CDDB,
                Some("cdstore") => PLAN_FILE_ID_CDSTORE,
                _ => continue,
            };
            line_indices.insert(fid, LineIndex::new(Arc::from(src.clone())));
        }
    }

    ProjectSnapshot {
        diagnostics_by_file,
        line_indices,
    }
}

/// One open document. The URI is the `HashMap` key in `Analyzer`;
/// the document holds the current source + version + cached
/// snapshot under a single `tokio::sync::Mutex` so updates and
/// reads serialize without blocking the LSP IO loop.
pub struct Document {
    /// Which dialect this document belongs to — fixed for the
    /// document's lifetime (derived from its URI extension on
    /// first open).
    kind: FileKind,
    inner: Mutex<DocumentInner>,
}

struct DocumentInner {
    version: i32,
    source: Arc<str>,
    snapshot: Option<Arc<Snapshot>>,
    /// `Some(cd_path)` if this document is a member of a project.
    /// Used by `publish_diagnostics_for` to find the project without
    /// scanning the whole projects map.
    project: Option<PathBuf>,
}

pub struct Snapshot {
    pub source: Arc<str>,
    pub diagnostics: Vec<Diagnostic>,
    pub hints: Vec<TypeHint>,
    pub line_index: LineIndex,
    pub version: i32,
}

/// One discovered project: a `.cd` entry point plus the same-
/// directory companion `.cddb` / `.cdstore` files. Members appear in
/// `members` once they've been opened in the editor; the project
/// entry persists as long as at least one member is open.
pub struct Project {
    pub cd_path: PathBuf,
    inner: Mutex<ProjectInner>,
}

struct ProjectInner {
    /// Cached database name parsed from the `.cd`'s
    /// `database <name>;` binding. Populated on .cd discovery and
    /// refreshed on .cd `did_change`.
    database_name: Option<String>,
    /// Buffer URIs for every open project member, keyed by their
    /// role in the plan (`FileId(0)` = `.cd`, `FileId(1)` = `.cddb`,
    /// `FileId(2)` = `.cdstore`).
    members: HashMap<FileId, Url>,
    /// Last computed plan snapshot. Cleared whenever any member's
    /// version changes or membership changes; the next request
    /// recomputes.
    snapshot: Option<Arc<ProjectSnapshot>>,
}

/// One computed cross-file plan snapshot: every PL diagnostic the
/// plan layer emitted, routed by [`FileId`] so
/// `publish_diagnostics_for` can hand each one off to the correct
/// member buffer.
pub struct ProjectSnapshot {
    pub diagnostics_by_file: HashMap<FileId, Vec<Diagnostic>>,
    pub line_indices: HashMap<FileId, LineIndex>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[tokio::test]
    async fn snapshot_caches_per_version() {
        let analyzer = Analyzer::new();
        let uri = url("file:///test.cd");
        analyzer
            .put_document(uri.clone(), 1, "oper main {} [];".to_string())
            .await;
        let s1 = analyzer.snapshot(&uri).await.unwrap();
        let s2 = analyzer.snapshot(&uri).await.unwrap();
        assert!(Arc::ptr_eq(&s1, &s2), "expected cached snapshot");
        assert_eq!(s1.version, 1);
    }

    #[tokio::test]
    async fn did_change_invalidates_snapshot() {
        let analyzer = Analyzer::new();
        let uri = url("file:///test.cd");
        analyzer
            .put_document(uri.clone(), 1, "oper main {} [];".to_string())
            .await;
        let s1 = analyzer.snapshot(&uri).await.unwrap();
        analyzer
            .put_document(uri.clone(), 2, "oper main {} [ let x = 1; ];".to_string())
            .await;
        let s2 = analyzer.snapshot(&uri).await.unwrap();
        assert!(!Arc::ptr_eq(&s1, &s2), "expected new snapshot");
        assert_eq!(s1.version, 1);
        assert_eq!(s2.version, 2);
        // The new snapshot must reflect the new source.
        assert!(s2
            .hints
            .iter()
            .any(|h| matches!(h.ty, coddl_types::Type::Integer)));
    }

    #[tokio::test]
    async fn closed_document_returns_none() {
        let analyzer = Analyzer::new();
        let uri = url("file:///test.cd");
        analyzer
            .put_document(uri.clone(), 1, "oper main {} [];".to_string())
            .await;
        analyzer.close_document(&uri).await;
        assert!(analyzer.snapshot(&uri).await.is_none());
    }

    #[tokio::test]
    async fn cddb_document_runs_parse_only() {
        // Opening a `.cddb` URI should produce a snapshot whose
        // diagnostics come from the parser. With well-formed source,
        // diagnostics are empty and hints stay empty (typecheck
        // doesn't run for dialects yet).
        let analyzer = Analyzer::new();
        let uri = url("file:///test.cddb");
        analyzer
            .put_document(
                uri.clone(),
                1,
                "database d;\nbase relvar X { id: Integer } key { id };\n".to_string(),
            )
            .await;
        let snap = analyzer.snapshot(&uri).await.unwrap();
        assert!(
            snap.diagnostics.is_empty(),
            "expected no diagnostics, got {:?}",
            snap.diagnostics
        );
        assert!(snap.hints.is_empty(), "dialect docs produce no hints today");
    }

    #[tokio::test]
    async fn cddb_document_surfaces_parser_diagnostic() {
        // A malformed `.cddb` produces a parser diagnostic (PB-code)
        // through the cached snapshot path.
        let analyzer = Analyzer::new();
        let uri = url("file:///test.cddb");
        analyzer
            .put_document(
                uri.clone(),
                1,
                "base relvar X {};\n".to_string(), // missing `database` header
            )
            .await;
        let snap = analyzer.snapshot(&uri).await.unwrap();
        assert!(
            snap.diagnostics.iter().any(|d| d.code.starts_with("PB")),
            "expected a PB-prefixed diagnostic, got {:?}",
            snap.diagnostics
        );
    }

    // ── project operator (frontend serves the LSP too) ──────────

    #[tokio::test]
    async fn project_type_error_surfaces_as_diagnostic() {
        // A bad `project` attribute must surface as a snapshot
        // diagnostic (T0027) through the same analyze path the CLI uses.
        let analyzer = Analyzer::new();
        let uri = url("file:///proj.cd");
        analyzer
            .put_document(
                uri.clone(),
                1,
                "oper main {} [ let s = Relation { {a: 1} } project {nope}; ];".to_string(),
            )
            .await;
        let snap = analyzer.snapshot(&uri).await.unwrap();
        assert!(
            snap.diagnostics.iter().any(|d| d.code == "T0027"),
            "expected T0027, got {:?}",
            snap.diagnostics
        );
    }

    #[tokio::test]
    async fn project_narrows_inlay_hint_heading() {
        // The inlay hint on a `project`ed binding reflects the narrowed
        // heading — it flows from `coddl_types` for free, no LSP wiring.
        let analyzer = Analyzer::new();
        let uri = url("file:///proj.cd");
        analyzer
            .put_document(
                uri.clone(),
                1,
                "oper main {} [ let s = Relation { {id: 1, message: \"x\"} } project {message}; ];"
                    .to_string(),
            )
            .await;
        let snap = analyzer.snapshot(&uri).await.unwrap();
        assert!(
            snap.diagnostics.is_empty(),
            "expected clean typecheck, got {:?}",
            snap.diagnostics
        );
        assert!(
            snap.hints.iter().any(|h| matches!(
                &h.ty,
                coddl_types::Type::Relation(hd)
                    if hd.lookup("message").is_some() && hd.lookup("id").is_none()
            )),
            "expected a narrowed Relation {{message}} hint, got {:?}",
            snap.hints
                .iter()
                .map(|h| format!("{}", h.ty))
                .collect::<Vec<_>>()
        );
    }

    // ── Project model tests ─────────────────────────────────────

    use std::fs;
    use tempfile::TempDir;

    const CD_HELLO: &str = "\
program hello;
database greetings;
public relvar Greetings { id: Integer, message: Text } key { id };
oper main {} [];
";
    const CDDB_OK: &str = "\
database greetings;
base relvar Greetings { id: Integer, message: Text } key { id };
";
    const CDDB_BAD: &str = "\
database greetings;
base relvar Greetings { id: Integer, message: Boolean } key { id };
";
    const CDSTORE_OK: &str = "\
store for greetings;
backend sqlite { file: \"greetings.sqlite\" };
relvar Greetings: table \"greetings\" {
    columns: { id: \"id\", message: \"message\" }
};
";

    fn url_from(path: &Path) -> Url {
        Url::from_file_path(path).expect("file path url")
    }

    fn write_project_files(dir: &Path) {
        fs::write(dir.join("app.cd"), CD_HELLO).unwrap();
        fs::write(dir.join("greetings.cddb"), CDDB_OK).unwrap();
        fs::write(dir.join("greetings.cdstore"), CDSTORE_OK).unwrap();
    }

    #[tokio::test]
    async fn opening_cd_registers_project() {
        let dir = TempDir::new().unwrap();
        write_project_files(dir.path());
        let analyzer = Analyzer::new();
        let cd_uri = url_from(&dir.path().join("app.cd"));

        analyzer
            .put_document(cd_uri.clone(), 1, CD_HELLO.to_string())
            .await;

        let project = analyzer.project_for(&cd_uri).await.expect("project");
        assert_eq!(project.cd_path, dir.path().join("app.cd"));
        let members = analyzer.project_members(&project).await;
        assert!(members.contains(&cd_uri));
    }

    #[tokio::test]
    async fn opening_cddb_first_creates_project_from_disk_scan() {
        let dir = TempDir::new().unwrap();
        write_project_files(dir.path());
        let analyzer = Analyzer::new();
        let cddb_uri = url_from(&dir.path().join("greetings.cddb"));

        analyzer
            .put_document(cddb_uri.clone(), 1, CDDB_OK.to_string())
            .await;

        let project = analyzer
            .project_for(&cddb_uri)
            .await
            .expect("project from reverse discovery");
        assert_eq!(project.cd_path, dir.path().join("app.cd"));
    }

    #[tokio::test]
    async fn opening_companion_after_cd_attaches() {
        let dir = TempDir::new().unwrap();
        write_project_files(dir.path());
        let analyzer = Analyzer::new();
        let cd_uri = url_from(&dir.path().join("app.cd"));
        let cddb_uri = url_from(&dir.path().join("greetings.cddb"));

        analyzer
            .put_document(cd_uri.clone(), 1, CD_HELLO.to_string())
            .await;
        analyzer
            .put_document(cddb_uri.clone(), 1, CDDB_OK.to_string())
            .await;

        let project = analyzer.project_for(&cddb_uri).await.expect("project");
        let members = analyzer.project_members(&project).await;
        assert!(members.contains(&cd_uri));
        assert!(members.contains(&cddb_uri));
    }

    #[tokio::test]
    async fn project_snapshot_routes_pl0007_to_cd_buffer() {
        let dir = TempDir::new().unwrap();
        // Disk has the matching .cddb; we'll feed a mismatched one
        // via the open buffer.
        write_project_files(dir.path());
        let analyzer = Analyzer::new();
        let cd_uri = url_from(&dir.path().join("app.cd"));
        let cddb_uri = url_from(&dir.path().join("greetings.cddb"));

        analyzer
            .put_document(cd_uri.clone(), 1, CD_HELLO.to_string())
            .await;
        analyzer
            .put_document(cddb_uri.clone(), 1, CDDB_BAD.to_string())
            .await;

        let project = analyzer.project_for(&cd_uri).await.unwrap();
        let psnap = analyzer
            .project_snapshot(&project.cd_path)
            .await
            .expect("project snapshot");

        // PL0007 (heading mismatch) is routed to .cd's FileId(0).
        let cd_diags = psnap
            .diagnostics_by_file
            .get(&FileId(0))
            .expect("CD diags");
        assert!(cd_diags.iter().any(|d| d.code == "PL0007"));
    }

    #[tokio::test]
    async fn edit_invalidates_project_snapshot() {
        let dir = TempDir::new().unwrap();
        write_project_files(dir.path());
        let analyzer = Analyzer::new();
        let cd_uri = url_from(&dir.path().join("app.cd"));
        let cddb_uri = url_from(&dir.path().join("greetings.cddb"));

        analyzer
            .put_document(cd_uri.clone(), 1, CD_HELLO.to_string())
            .await;
        analyzer
            .put_document(cddb_uri.clone(), 1, CDDB_OK.to_string())
            .await;

        let project = analyzer.project_for(&cd_uri).await.unwrap();
        let snap1 = analyzer.project_snapshot(&project.cd_path).await.unwrap();
        // Edit .cddb to introduce the mismatch.
        analyzer
            .put_document(cddb_uri.clone(), 2, CDDB_BAD.to_string())
            .await;
        let snap2 = analyzer.project_snapshot(&project.cd_path).await.unwrap();
        assert!(!Arc::ptr_eq(&snap1, &snap2), "expected fresh snapshot");
        let cd_diags = snap2.diagnostics_by_file.get(&FileId(0)).unwrap();
        assert!(cd_diags.iter().any(|d| d.code == "PL0007"));
    }

    #[tokio::test]
    async fn standalone_cd_creates_no_project() {
        let dir = TempDir::new().unwrap();
        // A .cd with no public relvars and no companions.
        let standalone = "program p;\noper main {} [];\n";
        let cd_path = dir.path().join("standalone.cd");
        fs::write(&cd_path, standalone).unwrap();
        let analyzer = Analyzer::new();
        let uri = url_from(&cd_path);

        analyzer
            .put_document(uri.clone(), 1, standalone.to_string())
            .await;

        // A project IS registered for the .cd (we always do that on
        // .cd open), but it has no companions and no plan
        // diagnostics. The project_snapshot should be empty.
        let project = analyzer.project_for(&uri).await.unwrap();
        let psnap = analyzer.project_snapshot(&project.cd_path).await.unwrap();
        assert!(
            psnap
                .diagnostics_by_file
                .values()
                .all(|v| v.is_empty()),
            "standalone .cd should have no plan diagnostics: {:?}",
            psnap.diagnostics_by_file
        );
    }

    #[tokio::test]
    async fn closing_last_member_drops_project() {
        let dir = TempDir::new().unwrap();
        write_project_files(dir.path());
        let analyzer = Analyzer::new();
        let cd_uri = url_from(&dir.path().join("app.cd"));

        analyzer
            .put_document(cd_uri.clone(), 1, CD_HELLO.to_string())
            .await;
        assert!(analyzer.project_for(&cd_uri).await.is_some());

        analyzer.close_document(&cd_uri).await;

        // After close, the project map should no longer hold this
        // project's cd_path.
        let projects = analyzer.projects.read().await;
        assert!(projects.is_empty(), "expected project torn down on last close");
    }

    #[tokio::test]
    async fn snapshot_after_put_uses_new_source() {
        // Verifies the "no stale data" property: if version-2 was
        // put while a version-1 snapshot was in flight, callers
        // after version-2 see version-2 data.
        let analyzer = Arc::new(Analyzer::new());
        let uri = url("file:///test.cd");
        analyzer
            .put_document(uri.clone(), 1, "oper main {} [];".to_string())
            .await;
        // Force the cache to be populated at v=1.
        let _ = analyzer.snapshot(&uri).await.unwrap();
        // Now bump to v=2 with different source.
        analyzer
            .put_document(uri.clone(), 2, "oper f {} [];".to_string())
            .await;
        let s = analyzer.snapshot(&uri).await.unwrap();
        assert_eq!(s.version, 2);
        // The new source's program name is empty (no program decl),
        // but the function name surfaces in hint emission only for
        // non-`main` opers' bodies. Easier signal: just check the
        // source string was carried into the snapshot.
        assert!(s.source.contains("oper f"));
    }
}
