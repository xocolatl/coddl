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
use std::sync::Arc;

use coddl_diagnostics::{Diagnostic, FileId};
use coddl_types::TypeHint;
use tokio::sync::{Mutex, RwLock};
use tower_lsp::lsp_types::Url;

use crate::line_index::LineIndex;

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
    /// cached snapshot — the next `snapshot()` recomputes.
    pub async fn put_document(&self, uri: Url, version: i32, source: String) {
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
    pub async fn snapshot(&self, uri: &Url) -> Option<Arc<Snapshot>> {
        let doc = {
            let files = self.files.read().await;
            files.get(uri).cloned()?
        };

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
        // alive for the `LineIndex` to share. The `CheckOutput`'s
        // `tree` field carries a `SyntaxNode` that's `!Sync` (rowan
        // uses an `Rc` internally for the cursor view), so we
        // decompose the result here and store only the Send/Sync
        // pieces. When a future feature needs the parsed tree
        // (semantic tokens, hover), the right move is to also store
        // the underlying `GreenNode` (which *is* Sync) and
        // reconstitute the `SyntaxNode` per request.
        let source_for_blocking = source.clone();
        let snap_arc = tokio::task::spawn_blocking(move || {
            let check = coddl_types::check(&source_for_blocking, FileId(0));
            let line_index = LineIndex::new(source_for_blocking.clone());
            Arc::new(Snapshot {
                source: source_for_blocking,
                diagnostics: check.diagnostics,
                hints: check.hints,
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
        let uri = url("file:///test.cdl");
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
        let uri = url("file:///test.cdl");
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
        let uri = url("file:///test.cdl");
        analyzer
            .put_document(uri.clone(), 1, "oper main {} [];".to_string())
            .await;
        analyzer.close_document(&uri).await;
        assert!(analyzer.snapshot(&uri).await.is_none());
    }

    #[tokio::test]
    async fn snapshot_after_put_uses_new_source() {
        // Verifies the "no stale data" property: if version-2 was
        // put while a version-1 snapshot was in flight, callers
        // after version-2 see version-2 data.
        let analyzer = Arc::new(Analyzer::new());
        let uri = url("file:///test.cdl");
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
