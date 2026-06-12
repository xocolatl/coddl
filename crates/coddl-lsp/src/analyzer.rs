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
use std::path::Path;
use std::sync::Arc;

use coddl_diagnostics::{Diagnostic, FileId};
use coddl_syntax::FileKind;
use coddl_types::TypeHint;
use tokio::sync::{Mutex, RwLock};
use tower_lsp::lsp_types::Url;

use crate::line_index::LineIndex;

/// Derive the dialect for a document from its URI's path extension.
/// Unrecognized extensions default to [`FileKind::Cd`] so the rest of
/// the pipeline stays uniform.
fn kind_from_uri(uri: &Url) -> FileKind {
    FileKind::from_path(Path::new(uri.path())).unwrap_or(FileKind::Cd)
}

pub struct Analyzer {
    files: RwLock<HashMap<Url, Arc<Document>>>,
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
        }
    }

    /// Insert or replace the document at `uri`. Invalidates any
    /// cached snapshot — the next `snapshot()` recomputes. The
    /// document's [`FileKind`] is resolved from `uri`'s extension on
    /// first insert and stays fixed for the document's lifetime.
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
                    uri,
                    Arc::new(Document {
                        kind,
                        inner: Mutex::new(DocumentInner {
                            version,
                            source,
                            snapshot: None,
                        }),
                    }),
                );
            }
        }
    }

    pub async fn close_document(&self, uri: &Url) {
        let mut files = self.files.write().await;
        files.remove(uri);
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
                FileKind::Cd => {
                    let check = coddl_types::check(&source_for_blocking, FileId(0));
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
}

pub struct Snapshot {
    pub source: Arc<str>,
    pub diagnostics: Vec<Diagnostic>,
    pub hints: Vec<TypeHint>,
    pub line_index: LineIndex,
    pub version: i32,
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
