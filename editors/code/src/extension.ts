import {
  commands,
  ExtensionContext,
  MarkdownString,
  OutputChannel,
  StatusBarAlignment,
  window,
} from 'vscode';
import { version } from '../package.json';

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

  statusBar(ctx);
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

function statusBar(ctx: ExtensionContext) {
  const statusBarItem = window.createStatusBarItem(
    StatusBarAlignment.Left,
    100
  );
  statusBarItem.text = 'greycat-analyzer';
  statusBarItem.tooltip = new MarkdownString(
    [
      `Extension: ${version}`,
      '',
      '---',
      '',
      '[$(refresh) Restart](command:greycat-analyzer.restart)  ',
      '',
      '[Need help?](https://doc.greycat.io)',
    ].join('\n'),
    true
  );
  // Required to make links clickable
  statusBarItem.tooltip.isTrusted = true;
  statusBarItem.command = 'greycat-analyzer.restart';
  // statusBarItem.show();

  function updateStatusBarVisiblity() {
    const editor = window.activeTextEditor;
    if (!editor) {
      return;
    }
    if (editor.document.languageId === 'greycat') {
      statusBarItem.show();
    } else {
      statusBarItem.hide();
    }
  }

  ctx.subscriptions.push(
    statusBarItem,
    window.onDidChangeActiveTextEditor(updateStatusBarVisiblity),
    window.onDidChangeVisibleTextEditors(updateStatusBarVisiblity)
  );
}
