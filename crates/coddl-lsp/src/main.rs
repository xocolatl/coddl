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
    ///
    /// Merges two diagnostic sources: per-document parse/typecheck
    /// diagnostics from `analyzer.snapshot`, and per-project plan
    /// diagnostics from `analyzer.project_snapshot` routed to this
    /// URI's `FileId`. Standalone documents (no project) see only
    /// per-document diagnostics, preserving Phase 13 behavior.
    async fn publish_diagnostics_for(&self, uri: &Url) {
        let Some(snap) = self.analyzer.snapshot(uri).await else {
            return;
        };

        // The plan pass re-typechecks every project member, so its per-file
        // diagnostics duplicate the document's own typecheck. Gather its
        // diagnostics for this URI's role; `published_diagnostics` keeps only
        // the new plan-level (`PL####`) ones, so nothing reports twice. All of
        // them are in this file's coordinates, so `snap.line_index` converts
        // them.
        let plan_diags = match self.analyzer.project_for(uri).await {
            Some(project) => match self.analyzer.project_snapshot(&project.cd_path).await {
                Some(psnap) => match self.analyzer.file_id_for(&project, uri).await {
                    Some(fid) => psnap.diagnostics_by_file.get(&fid).cloned().unwrap_or_default(),
                    None => Vec::new(),
                },
                None => Vec::new(),
            },
            None => Vec::new(),
        };

        let diagnostics: Vec<_> =
            analyzer::published_diagnostics(&snap.diagnostics, &plan_diags)
                .iter()
                .map(|d| lsp_convert::diagnostic(d, &snap.line_index))
                .collect();

        self.client
            .publish_diagnostics(uri.clone(), diagnostics, Some(snap.version))
            .await;
    }

    /// Republish diagnostics for every open project member that
    /// shares a project with `uri`. Called after `did_change` so an
    /// edit to `.cddb` refreshes the `.cd` buffer's plan squiggles.
    async fn republish_project_members(&self, uri: &Url) {
        let Some(project) = self.analyzer.project_for(uri).await else {
            return;
        };
        let members = self.analyzer.project_members(&project).await;
        for member_uri in members {
            if &member_uri == uri {
                continue; // already published above
            }
            self.publish_diagnostics_for(&member_uri).await;
        }
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
        // A new member may have attached to a project; refresh
        // diagnostics for the other members so their plan squiggles
        // reflect this file's presence.
        self.republish_project_members(&uri).await;
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
        // Plan diagnostics for sibling members may have moved; fan
        // out an updated publish so e.g. editing greetings.cddb
        // refreshes hello-world-db.cd's squiggles.
        self.republish_project_members(&uri).await;
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
        let kind = analyzer::kind_from_uri(&params.text_document.uri);
        let out = coddl_fmt::format(&snap.source, &coddl_fmt::FormatOptions::default(), kind);
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
