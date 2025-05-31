import { commands, ExtensionContext } from 'vscode';

import {
  Executable,
  LanguageClient,
  TransportKind,
} from 'vscode-languageclient/node';

let client: LanguageClient | null = null;

export function activate(ctx: ExtensionContext) {
  ctx.subscriptions.push(
    commands.registerCommand('greycat-analyzer.restart', restart)
  );

  startClient();
}

export function deactivate(): Thenable<void> | undefined {
  if (!client) {
    return;
  }
  return client.stop();
}

async function restart() {
  if (client === null) {
    await startClient();
    return;
  }
  await client.stop();
  await startClient();
  return;
}

function startClient() {
  client = new LanguageClient(
    'greycat-analyzer',
    'greycat-analyzer',
    {
      run: {
        command: 'greycat-analyzer',
        args: ['lang-server'],
        transport: TransportKind.stdio,
      } satisfies Executable,
      debug: {
        command: 'greycat-analyzer',
        args: ['lang-server'],
        transport: TransportKind.stdio,
      } satisfies Executable,
    },
    {
      documentSelector: [{ scheme: 'file', language: 'gcl' }],
    }
  );

  return client.start();
}
