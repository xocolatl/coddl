// Coddl VSCode extension — language client for coddl-lsp.
//
// Activates on the `coddl` language id, reads the server binary path
// from the `coddl.lsp.path` setting (defaults to `coddl-lsp`, resolved
// against $PATH), and spawns it over stdio.

import * as vscode from 'vscode';
import {
    LanguageClient,
    LanguageClientOptions,
    ServerOptions,
    TransportKind,
} from 'vscode-languageclient/node';

let client: LanguageClient | undefined;

export async function activate(context: vscode.ExtensionContext): Promise<void> {
    const serverPath = vscode.workspace
        .getConfiguration('coddl')
        .get<string>('lsp.path', 'coddl-lsp');

    const serverOptions: ServerOptions = {
        run: { command: serverPath, transport: TransportKind.stdio },
        debug: { command: serverPath, transport: TransportKind.stdio },
    };

    const clientOptions: LanguageClientOptions = {
        documentSelector: [
            { scheme: 'file', language: 'coddl' },
            { scheme: 'file', language: 'coddl-cddb' },
            { scheme: 'file', language: 'coddl-cdmap' },
            { scheme: 'file', language: 'coddl-cdstore' },
        ],
        synchronize: {
            fileEvents: vscode.workspace.createFileSystemWatcher(
                '**/*.{cdl,cddb,cdmap,cdstore}',
            ),
        },
    };

    client = new LanguageClient(
        'coddl',
        'Coddl Language Server',
        serverOptions,
        clientOptions,
    );

    try {
        await client.start();
    } catch (err) {
        const message =
            err instanceof Error ? err.message : String(err);
        void vscode.window.showErrorMessage(
            `coddl-lsp failed to start (path: "${serverPath}"). ${message}`,
        );
    }

    context.subscriptions.push({
        dispose: () => {
            // handled in deactivate
        },
    });
}

export async function deactivate(): Promise<void> {
    if (client) {
        await client.stop();
        client = undefined;
    }
}
