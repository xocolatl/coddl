//! `coddl-lsp` — the Coddl language server.
//!
//! Thin `tower-lsp` adapter: owns document state and request dispatch,
//! delegates analysis to the frontend crates. Capabilities today:
//! syntax highlighting (driven by the VSCode extension's TextMate
//! grammar), document formatting via `coddl-fmt`, and inlay hints
//! surfacing inferred types from `coddl-types`. Diagnostics streaming,
//! semantic tokens, hover, and completion all reuse the same
//! document state + frontend pipeline when they land.

mod line_index;

use std::collections::HashMap;

use coddl_diagnostics::FileId;
use tokio::sync::RwLock;
use tower_lsp::jsonrpc::Result as LspResult;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer, LspService, Server};

use crate::line_index::LineIndex;

/// Per-document state. Today just the source text — the typechecker
/// re-runs on each request, which is fast enough for hello-world-
/// sized files. When perf matters, this is where the parsed CST,
/// type tables, and incremental-recompile state move in.
struct CoddlLsp {
    client: Client,
    documents: RwLock<HashMap<Url, String>>,
}

impl CoddlLsp {
    async fn put_document(&self, uri: Url, text: String) {
        let mut docs = self.documents.write().await;
        docs.insert(uri, text);
    }

    async fn remove_document(&self, uri: &Url) {
        let mut docs = self.documents.write().await;
        docs.remove(uri);
    }

    async fn read_document(&self, uri: &Url) -> Option<String> {
        let docs = self.documents.read().await;
        docs.get(uri).cloned()
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
        self.put_document(params.text_document.uri, params.text_document.text)
            .await;
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        // We advertise `TextDocumentSyncKind::FULL`, so each change
        // event carries the entire buffer; the last one wins.
        if let Some(change) = params.content_changes.into_iter().last() {
            self.put_document(params.text_document.uri, change.text)
                .await;
        }
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        self.remove_document(&params.text_document.uri).await;
    }

    // ── Formatting ──────────────────────────────────────────────────

    async fn formatting(
        &self,
        params: DocumentFormattingParams,
    ) -> LspResult<Option<Vec<TextEdit>>> {
        let source = match self.read_document(&params.text_document.uri).await {
            Some(s) => s,
            None => return Ok(None),
        };
        let out = coddl_fmt::format(&source, &coddl_fmt::FormatOptions::default());
        if out.text == source {
            return Ok(None);
        }
        // Replace the entire buffer. A future tightening compares
        // line-by-line and emits minimal edits.
        let line_count = source.lines().count().max(1) as u32;
        Ok(Some(vec![TextEdit {
            range: Range::new(Position::new(0, 0), Position::new(line_count, 0)),
            new_text: out.text,
        }]))
    }

    // ── Inlay hints ────────────────────────────────────────────────

    async fn inlay_hint(&self, params: InlayHintParams) -> LspResult<Option<Vec<InlayHint>>> {
        let source = match self.read_document(&params.text_document.uri).await {
            Some(s) => s,
            None => return Ok(None),
        };
        let check_out = coddl_types::check(&source, FileId(0));
        let line_index = LineIndex::new(&source);

        let hints: Vec<InlayHint> = check_out
            .hints
            .iter()
            .map(|h| {
                // Label prefix follows the surface syntax: `: T` for
                // a binding annotation, `-> T` for an operator return
                // clause. The leading space on `OperReturn` ghosts in
                // between the heading and the body.
                let label = match h.kind {
                    coddl_types::HintKind::LetBinding => format!(": {}", h.ty),
                    coddl_types::HintKind::OperReturn => format!(" -> {}", h.ty),
                };
                InlayHint {
                    position: line_index.position(h.span.start),
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
        documents: RwLock::new(HashMap::new()),
    });
    Server::new(stdin, stdout, socket).serve(service).await;
}
