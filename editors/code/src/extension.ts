import {
  commands,
  ExtensionContext,
  MarkdownString,
  OutputChannel,
  StatusBarAlignment,
  window,
  workspace,
} from 'vscode';
import { version } from '../package.json';

import { Executable, LanguageClient, TransportKind } from 'vscode-languageclient/node';

let channel: OutputChannel;
let client: LanguageClient | null = null;

export function activate(ctx: ExtensionContext) {
  channel = window.createOutputChannel('greycat-analyzer');

  ctx.subscriptions.push(
    channel,
    commands.registerCommand('greycat-analyzer.restart', restart),
    workspace.onDidChangeConfiguration(async (e) => {
      // Both settings require an LSP restart: trace.server changes
      // RUST_LOG (only read at startup) and lintLibs is sent via
      // initializationOptions (only consumed on `initialize`).
      const traceChanged = e.affectsConfiguration('greycat-analyzer.trace.server');
      const lintLibsChanged = e.affectsConfiguration('greycat-analyzer.lintLibs');
      if (!traceChanged && !lintLibsChanged) {
        return;
      }
      const what = traceChanged ? 'log level' : 'lintLibs';
      const choice = await window.showInformationMessage(
        `greycat-analyzer ${what} changed. Restart the server now?`,
        'Restart',
        'Later',
      );
      if (choice === 'Restart') {
        await restart();
      }
    }),
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
    args: ['server'],
    transport: TransportKind.stdio,
    options: {
      env: {
        // Defaults first, user env last — so a power user who sets
        // RUST_BACKTRACE=full or RUST_LOG=trace in their shell wins.
        // RUST_BACKTRACE=1 surfaces a usable backtrace in the output
        // channel for any LSP-side panic (zero cost otherwise).
        RUST_BACKTRACE: '1',
        RUST_LOG: buildRustLog(),
        ...process.env,
      },
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
      initializationOptions: buildInitializationOptions(),
    },
  );

  return client.start();
}

/**
 * Build the JSON object sent to the server via `initializationOptions`.
 * Mirror every Rust-side `Backend` field that's user-configurable here
 * so the server can read them once on `initialize` rather than running
 * a stream of `workspace/configuration` requests.
 */
function buildInitializationOptions(): Record<string, unknown> {
  const cfg = workspace.getConfiguration('greycat-analyzer');
  return {
    lintLibs: cfg.get<boolean>('lintLibs', false),
  };
}

/**
 * Build the RUST_LOG value from the `greycat-analyzer.trace.server`
 * setting. The setting is one of `off | info | debug | trace`; the
 * corresponding log spec scopes the level to the analyzer's own
 * crates so external dependencies stay quiet at every tier.
 */
function buildRustLog(): string {
  const cfg = workspace.getConfiguration('greycat-analyzer');
  const level = cfg.get<string>('trace.server', 'info');
  if (level === 'off') {
    return 'off';
  }
  return [
    `greycat_analyzer_server=${level}`,
    `greycat_analyzer_core=${level}`,
    `greycat_analyzer_analysis=${level}`,
  ].join(',');
}

function statusBar(ctx: ExtensionContext) {
  const statusBarItem = window.createStatusBarItem(StatusBarAlignment.Left, 100);
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
    true,
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
    window.onDidChangeVisibleTextEditors(updateStatusBarVisiblity),
  );
}
