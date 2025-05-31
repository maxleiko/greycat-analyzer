import { commands, ExtensionContext, OutputChannel, window } from 'vscode';

import {
  Executable,
  LanguageClient,
  TransportKind,
} from 'vscode-languageclient/node';

let channel: OutputChannel;
let client: LanguageClient | null = null;

export function activate(ctx: ExtensionContext) {
  channel = window.createOutputChannel('greycat-analyzer');

  ctx.subscriptions.push(
    channel,
    commands.registerCommand('greycat-analyzer.restart', restart)
  );

  startClient();
}

export function deactivate(): Thenable<void> | undefined {
  if (client === null) {
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
  const run: Executable = {
    command: 'greycat-analyzer',
    args: ['lang-server'],
    transport: TransportKind.stdio,
    options: {
      env: { ...process.env, RUST_LOG: 'debug' },
    },
  };

  client = new LanguageClient(
    'greycat-analyzer',
    {
      run,
      debug: run,
    },
    {
      outputChannel: channel,
      documentSelector: [{ scheme: 'file', language: 'greycat' }],
    }
  );

  return client.start();
}
