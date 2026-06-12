//! `coddl-lsp` — the Coddl language server.
//!
//! Thin `tower-lsp` adapter over an `Analyzer` (per-document
//! analysis cache + version tracking + threaded compute). Today's
//! capabilities: document sync, formatting via `coddl-fmt`,
//! inferred-type inlay hints, and push-on-edit diagnostics. Each
//! future feature (hover, go-to-def, completion, semantic tokens)
//! lands as a handler that calls `analyzer.snapshot(uri)` and
//! reads what it needs.

mod analyzer;
mod line_index;
mod lsp_convert;

use std::sync::Arc;

use tower_lsp::jsonrpc::Result as LspResult;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer, LspService, Server};

use crate::analyzer::Analyzer;

struct CoddlLsp {
    client: Client,
    analyzer: Arc<Analyzer>,
}

impl CoddlLsp {
    /// Compute the snapshot for `uri` and push its diagnostics to
    /// the client. Used by `did_open` and `did_change` to surface
    /// errors as the user types.
    async fn publish_diagnostics_for(&self, uri: &Url) {
        let Some(snap) = self.analyzer.snapshot(uri).await else {
            return;
        };
        let diagnostics: Vec<_> = snap
            .diagnostics
            .iter()
            .map(|d| lsp_convert::diagnostic(d, &snap.line_index))
            .collect();
        self.client
            .publish_diagnostics(uri.clone(), diagnostics, Some(snap.version))
            .await;
    }
}

#[tower_lsp::async_trait]
impl LanguageServer for CoddlLsp {
    async fn initialize(&self, _: InitializeParams) -> LspResult<InitializeResult> {
        Ok(InitializeResult {
            server_info: Some(ServerInfo {
                name: "coddl-lsp".into(),
                version: Some(env!("CARGO_PKG_VERSION").into()),
            }),
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::FULL,
                )),
                document_formatting_provider: Some(OneOf::Left(true)),
                inlay_hint_provider: Some(OneOf::Left(true)),
                ..ServerCapabilities::default()
            },
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        self.client
            .log_message(MessageType::INFO, "coddl-lsp initialized")
            .await;
    }

    async fn shutdown(&self) -> LspResult<()> {
        Ok(())
    }

    // ── Document sync ───────────────────────────────────────────────

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let uri = params.text_document.uri.clone();
        self.analyzer
            .put_document(
                params.text_document.uri,
                params.text_document.version,
                params.text_document.text,
            )
            .await;
        self.publish_diagnostics_for(&uri).await;
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        // FULL sync mode: each change carries the whole buffer.
        let Some(change) = params.content_changes.into_iter().last() else {
            return;
        };
        let uri = params.text_document.uri.clone();
        self.analyzer
            .put_document(
                params.text_document.uri,
                params.text_document.version,
                change.text,
            )
            .await;
        self.publish_diagnostics_for(&uri).await;
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let uri = params.text_document.uri;
        self.analyzer.close_document(&uri).await;
        // Clear any lingering squiggles from the editor's view.
        self.client.publish_diagnostics(uri, Vec::new(), None).await;
    }

    // ── Formatting ──────────────────────────────────────────────────

    async fn formatting(
        &self,
        params: DocumentFormattingParams,
    ) -> LspResult<Option<Vec<TextEdit>>> {
        let Some(snap) = self.analyzer.snapshot(&params.text_document.uri).await else {
            return Ok(None);
        };
        let out = coddl_fmt::format(&snap.source, &coddl_fmt::FormatOptions::default());
        if out.text.as_str() == snap.source.as_ref() {
            return Ok(None);
        }
        let line_count = snap.source.lines().count().max(1) as u32;
        Ok(Some(vec![TextEdit {
            range: Range::new(Position::new(0, 0), Position::new(line_count, 0)),
            new_text: out.text,
        }]))
    }

    // ── Inlay hints ────────────────────────────────────────────────

    async fn inlay_hint(&self, params: InlayHintParams) -> LspResult<Option<Vec<InlayHint>>> {
        let Some(snap) = self.analyzer.snapshot(&params.text_document.uri).await else {
            return Ok(None);
        };
        let hints: Vec<InlayHint> = snap
            .hints
            .iter()
            .map(|h| {
                let label = match h.kind {
                    coddl_types::HintKind::LetBinding => format!(": {}", h.ty),
                    coddl_types::HintKind::OperReturn => format!(" -> {}", h.ty),
                };
                InlayHint {
                    position: snap.line_index.position(h.span.start),
                    label: InlayHintLabel::String(label),
                    kind: Some(InlayHintKind::TYPE),
                    tooltip: None,
                    padding_left: Some(false),
                    padding_right: Some(false),
                    text_edits: None,
                    data: None,
                }
            })
            .collect();
        Ok(Some(hints))
    }
}

#[tokio::main]
async fn main() {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    let (service, socket) = LspService::new(|client| CoddlLsp {
        client,
        analyzer: Arc::new(Analyzer::new()),
    });
    Server::new(stdin, stdout, socket).serve(service).await;
}
