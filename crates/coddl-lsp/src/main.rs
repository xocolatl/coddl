//! `coddl-lsp` — the Coddl language server.
//!
//! Thin `tower-lsp` adapter: owns document state and request dispatch,
//! delegates analysis to the frontend crates. Surface capabilities
//! today are syntax highlighting (driven by the VSCode extension's
//! TextMate grammar) plus diagnostics streamed from the frontend's
//! `Vec<Diagnostic>` output.

use tower_lsp::jsonrpc::Result as LspResult;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer, LspService, Server};

struct CoddlLsp {
    client: Client,
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
                ..ServerCapabilities::default()
            },
        })
    }

    async fn formatting(
        &self,
        params: DocumentFormattingParams,
    ) -> LspResult<Option<Vec<TextEdit>>> {
        // TODO: look up the document text from our state once `did_open`
        // / `did_change` populate it. For now we read the file off disk so
        // the LSP path through `coddl-fmt` is exercised end-to-end.
        let path = match params.text_document.uri.to_file_path() {
            Ok(p) => p,
            Err(_) => return Ok(None),
        };
        let source = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(_) => return Ok(None),
        };
        let out = coddl_fmt::format(&source, &coddl_fmt::FormatOptions::default());
        if out.text == source {
            return Ok(None);
        }
        let line_count = source.lines().count().max(1) as u32;
        Ok(Some(vec![TextEdit {
            range: Range::new(Position::new(0, 0), Position::new(line_count, 0)),
            new_text: out.text,
        }]))
    }

    async fn initialized(&self, _: InitializedParams) {
        self.client
            .log_message(MessageType::INFO, "coddl-lsp initialized")
            .await;
    }

    async fn shutdown(&self) -> LspResult<()> {
        Ok(())
    }

    async fn did_open(&self, _params: DidOpenTextDocumentParams) {
        // TODO: parse + typecheck the buffer; publish diagnostics.
    }

    async fn did_change(&self, _params: DidChangeTextDocumentParams) {
        // TODO: full re-parse + re-typecheck; incremental via salsa later.
    }
}

#[tokio::main]
async fn main() {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    let (service, socket) = LspService::new(|client| CoddlLsp { client });
    Server::new(stdin, stdout, socket).serve(service).await;
}
