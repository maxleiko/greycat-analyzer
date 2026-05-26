import { existsSync } from 'fs';

import {
  commands,
  ConfigurationTarget,
  Disposable,
  env,
  ExtensionContext,
  MarkdownString,
  OutputChannel,
  StatusBarAlignment,
  StatusBarItem,
  Uri,
  window,
  workspace,
} from 'vscode';
import { version } from '../package.json';

import { Executable, LanguageClient, TransportKind } from 'vscode-languageclient/node';

import {
  clearManagedBinary,
  downloadAndInstall,
  managedBinaryPath,
  probeVersion,
  resolveRedirect,
} from './install';

type BinarySource = 'settings' | 'path' | 'managed';

interface ResolvedBinary {
  /** Absolute path (or bare `greycat-analyzer` when source = `'path'`). */
  command: string;
  /** Where the binary came from. Drives the update-check prompt shape. */
  source: BinarySource;
  /** Output of `<bin> --version`, or `null` if it didn't respond. */
  version: string | null;
}

const REPO = 'maxleiko/greycat-analyzer';
const LATEST_RELEASE_URL = `https://github.com/${REPO}/releases/latest`;
const ANALYZER_VERSION_KEY = 'analyzerVersion';
const LAST_UPDATE_CHECK_KEY = 'lastUpdateCheckAt';
const SKIPPED_VERSION_KEY = 'skippedVersion';

let channel: OutputChannel;
let client: LanguageClient | null = null;
let updateTimer: ReturnType<typeof setInterval> | null = null;
let statusBarItem: StatusBarItem | null = null;
let resolvedBinary: ResolvedBinary | null = null;
let latestKnownTag: string | null = null;

export function activate(ctx: ExtensionContext) {
  channel = window.createOutputChannel('GreyCat');

  ctx.subscriptions.push(
    channel,
    commands.registerCommand('greycat.restartServer', restart),
    commands.registerCommand('greycat.downloadServer', () => downloadServerCmd(ctx)),
    commands.registerCommand('greycat.showServerPath', () => showServerPathCmd(ctx)),
    commands.registerCommand('greycat.checkForUpdates', () => checkForUpdatesCmd(ctx, true)),
    workspace.onDidChangeConfiguration(async (e) => {
      const traceChanged = e.affectsConfiguration('greycat.trace.server');
      const lintLibsChanged = e.affectsConfiguration('greycat.lintLibs');
      const debounceChanged = e.affectsConfiguration('greycat.diagnosticsDebounceMs');
      const serverPathChanged = e.affectsConfiguration('greycat.serverPath');
      const checkCadenceChanged = e.affectsConfiguration('greycat.checkForUpdates');

      if (checkCadenceChanged) {
        rescheduleUpdateCheck(ctx);
      }

      if (
        !traceChanged &&
        !lintLibsChanged &&
        !debounceChanged &&
        !serverPathChanged
      ) {
        return;
      }
      const what = traceChanged
        ? 'log level'
        : lintLibsChanged
          ? 'lintLibs'
          : debounceChanged
            ? 'diagnosticsDebounceMs'
            : 'serverPath';
      const choice = await window.showInformationMessage(
        `GreyCat ${what} changed. Restart the LSP server now?`,
        'Restart',
        'Later',
      );
      if (choice === 'Restart') {
        await restart(ctx);
      }
    }),
    new Disposable(() => disposeUpdateTimer()),
  );

  statusBarItem = makeStatusBar(ctx);
  void bootstrap(ctx);
}

export function deactivate(): Thenable<void> | undefined {
  disposeUpdateTimer();
  if (client === null) {
    return;
  }
  return client.stop();
}

async function bootstrap(ctx: ExtensionContext) {
  const binary = await resolveBinary(ctx);
  if (binary) {
    resolvedBinary = binary;
    persistVersion(ctx, binary.version);
    refreshStatusBar();
    channel.appendLine(`[startup] server=${binary.command} (${binary.source})`);
    if (binary.version) {
      channel.appendLine(`[startup] version=${binary.version}`);
    }
    await startClient(binary);
  } else {
    channel.appendLine('[startup] no greycat-analyzer found — prompting user');
    await promptFirstRun(ctx);
  }
  scheduleUpdateCheck(ctx);
}

/**
 * First-run prompt when no binary was discovered. Lets the user pick
 * between auto-install, pointing at their own binary, or dismissal.
 */
async function promptFirstRun(ctx: ExtensionContext) {
  const choice = await window.showInformationMessage(
    'GreyCat language services need the `greycat-analyzer` binary, which was not found on PATH. Install it now?',
    'Download',
    'Browse for binary…',
    'Cancel',
  );
  if (choice === 'Download') {
    await downloadServerCmd(ctx);
  } else if (choice === 'Browse for binary…') {
    await browseForBinary(ctx);
  }
}

/**
 * Open a file picker for the user to point at an existing
 * `greycat-analyzer` binary. Validates with `--version`, writes the
 * chosen path into `greycat.serverPath`, and restarts.
 */
async function browseForBinary(ctx: ExtensionContext) {
  const picks = await window.showOpenDialog({
    canSelectFiles: true,
    canSelectFolders: false,
    canSelectMany: false,
    openLabel: 'Use this binary',
    title: 'Select greycat-analyzer binary',
  });
  if (!picks || picks.length === 0) {
    return;
  }
  const chosen = picks[0].fsPath;
  const v = probeVersion(chosen);
  if (!v) {
    window.showErrorMessage(`Selected file does not respond to --version: ${chosen}`);
    return;
  }
  await workspace
    .getConfiguration('greycat')
    .update('serverPath', chosen, ConfigurationTarget.Global);
  await restart(ctx);
}

/**
 * Driver for the `greycat.downloadServer` command. Always redownloads
 * — even if a cached binary already exists — so it doubles as the
 * "force update now" path.
 */
async function downloadServerCmd(ctx: ExtensionContext) {
  try {
    clearManagedBinary(ctx);
    const { binaryPath, version: v } = await downloadAndInstall(ctx, channel);
    resolvedBinary = { command: binaryPath, source: 'managed', version: v };
    persistVersion(ctx, v);
    refreshStatusBar();
    await restart(ctx);
    window.showInformationMessage(`GreyCat analyzer installed (${v}).`);
  } catch (err) {
    const message = err instanceof Error ? err.message : String(err);
    channel.appendLine(`[install] failed: ${message}`);
    window.showErrorMessage(`GreyCat analyzer install failed: ${message}`);
  }
}

/**
 * Driver for the `greycat.showServerPath` command. Pops a message
 * with the resolved binary path, its source, its `--version` output,
 * and the latest known release tag (if we've ever checked).
 */
async function showServerPathCmd(ctx: ExtensionContext) {
  const b = resolvedBinary ?? (await resolveBinary(ctx));
  if (!b) {
    window.showWarningMessage('GreyCat: no analyzer binary is currently resolved.');
    return;
  }
  const parts = [
    `Server: \`${b.command}\``,
    `Source: ${b.source}`,
    b.version ? `Version: ${b.version}` : 'Version: (unknown)',
  ];
  if (latestKnownTag) {
    parts.push(`Latest known release: ${latestKnownTag}`);
  }
  channel.appendLine(`[show-path] ${parts.join(' | ')}`);
  channel.show(true);
}

/**
 * Driver for the explicit `greycat.checkForUpdates` command. Resets
 * the throttle so the probe always runs, even if the auto-check would
 * have skipped this tick.
 */
async function checkForUpdatesCmd(ctx: ExtensionContext, forced: boolean) {
  await ctx.globalState.update(LAST_UPDATE_CHECK_KEY, undefined);
  await runUpdateCheck(ctx, { force: forced, surfaceNoUpdate: forced });
}

async function startClient(binary: ResolvedBinary) {
  const run: Executable = {
    command: binary.command,
    args: ['server'],
    transport: TransportKind.stdio,
    options: {
      env: {
        // Defaults first, user env last — so a power user who sets
        // RUST_BACKTRACE=full or RUST_LOG=trace in their shell wins.
        RUST_BACKTRACE: '1',
        RUST_LOG: buildRustLog(),
        ...process.env,
      },
    },
  };

  client = new LanguageClient(
    'greycat',
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

  await client.start();
}

async function restart(ctx?: ExtensionContext) {
  if (client !== null) {
    await client.stop();
    client = null;
  }
  if (ctx) {
    const binary = await resolveBinary(ctx);
    if (binary) {
      resolvedBinary = binary;
      persistVersion(ctx, binary.version);
      refreshStatusBar();
      channel.appendLine(`[restart] server=${binary.command} (${binary.source})`);
      await startClient(binary);
      return;
    }
    channel.appendLine('[restart] no greycat-analyzer found');
    await promptFirstRun(ctx);
    return;
  }
  if (resolvedBinary) {
    await startClient(resolvedBinary);
  }
}

/**
 * Walk the discovery order described in
 * [`/home/leiko/.claude/plans/soft-frolicking-grove.md`]: settings
 * override → PATH lookup → managed cache. First hit wins; returns
 * `null` if nothing resolves (caller surfaces the prompt).
 */
async function resolveBinary(ctx: ExtensionContext): Promise<ResolvedBinary | null> {
  const cfg = workspace.getConfiguration('greycat');
  const override = (cfg.get<string>('serverPath') ?? '').trim();
  if (override) {
    if (existsSync(override)) {
      const v = probeVersion(override);
      if (v) {
        return { command: override, source: 'settings', version: v };
      }
      channel.appendLine(`[resolve] greycat.serverPath does not respond to --version: ${override}`);
    } else {
      channel.appendLine(`[resolve] greycat.serverPath does not exist: ${override}`);
    }
  }

  const v = probeVersion('greycat-analyzer');
  if (v) {
    return { command: 'greycat-analyzer', source: 'path', version: v };
  }

  const cached = managedBinaryPath(ctx);
  if (cached && existsSync(cached)) {
    const cv = probeVersion(cached);
    if (cv) {
      return { command: cached, source: 'managed', version: cv };
    }
  }

  return null;
}

function persistVersion(ctx: ExtensionContext, version: string | null) {
  void ctx.globalState.update(ANALYZER_VERSION_KEY, version);
}

function buildInitializationOptions(): Record<string, unknown> {
  const cfg = workspace.getConfiguration('greycat');
  return {
    lintLibs: cfg.get<boolean>('lintLibs', false),
    diagnosticsDebounceMs: cfg.get<number>('diagnosticsDebounceMs', 150),
  };
}

function buildRustLog(): string {
  const cfg = workspace.getConfiguration('greycat');
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

// ----------------------------------------------------------------------------
// Status bar
// ----------------------------------------------------------------------------

function makeStatusBar(ctx: ExtensionContext): StatusBarItem {
  const item = window.createStatusBarItem(StatusBarAlignment.Left, 100);
  item.text = 'GreyCat';
  item.command = 'greycat.restartServer';
  refreshStatusBar(item);

  function updateStatusBarVisiblity() {
    const editor = window.activeTextEditor;
    if (!editor) {
      return;
    }
    if (editor.document.languageId === 'greycat') {
      item.show();
    } else {
      item.hide();
    }
  }

  ctx.subscriptions.push(
    item,
    window.onDidChangeActiveTextEditor(updateStatusBarVisiblity),
    window.onDidChangeVisibleTextEditors(updateStatusBarVisiblity),
  );
  return item;
}

function refreshStatusBar(item: StatusBarItem | null = statusBarItem) {
  if (!item) {
    return;
  }
  // Markdown convention: trailing two spaces force a hard line break
  // inside a paragraph. Without it, sibling lines collapse into one
  // very-wide row in the tooltip.
  const br = '  ';
  const lines = [`Extension: ${version}${br}`];
  if (resolvedBinary) {
    lines.push('', '---', '', `Server (${resolvedBinary.source}): \`${resolvedBinary.command}\`${br}`);
    const v = stripBinaryNameFromVersion(resolvedBinary.version);
    if (v) {
      lines.push(`Version: ${v}${br}`);
    }
  } else {
    lines.push('', '---', '', `*No analyzer binary resolved.*${br}`);
  }
  if (latestKnownTag) {
    lines.push(`Latest: ${latestKnownTag}${br}`);
  }
  lines.push(
    '',
    '---',
    '',
    `[$(refresh) Restart](command:greycat.restartServer)${br}`,
    `[$(cloud-download) Update](command:greycat.checkForUpdates)${br}`,
    '',
    '[Need help?](https://doc.greycat.io)',
  );
  item.tooltip = new MarkdownString(lines.join('\n'), true);
  item.tooltip.isTrusted = true;
}

/**
 * `greycat-analyzer --version` prints `greycat-analyzer 0.1.4`. The
 * status-bar already shows the server path on its own line, so the
 * binary-name prefix is noise — strip it.
 */
function stripBinaryNameFromVersion(raw: string | null): string | null {
  if (!raw) {
    return null;
  }
  return raw.replace(/^greycat-analyzer\s+/, '').trim() || null;
}

// ----------------------------------------------------------------------------
// Periodic update check
// ----------------------------------------------------------------------------

type CheckCadence = 'off' | 'onStartup' | 'daily' | 'weekly';

function readCheckCadence(): CheckCadence {
  const cfg = workspace.getConfiguration('greycat');
  const raw = cfg.get<string>('checkForUpdates', 'daily');
  if (raw === 'off' || raw === 'onStartup' || raw === 'daily' || raw === 'weekly') {
    return raw;
  }
  return 'daily';
}

function intervalMsFor(cadence: CheckCadence): number | null {
  switch (cadence) {
    case 'daily':
      return 24 * 60 * 60 * 1000;
    case 'weekly':
      return 7 * 24 * 60 * 60 * 1000;
    default:
      return null;
  }
}

function disposeUpdateTimer() {
  if (updateTimer !== null) {
    clearInterval(updateTimer);
    updateTimer = null;
  }
}

function scheduleUpdateCheck(ctx: ExtensionContext) {
  const cadence = readCheckCadence();
  if (cadence === 'off') {
    channel.appendLine('[update-check] disabled');
    return;
  }
  // Activation-time probe (subject to throttle).
  void runUpdateCheck(ctx, { force: false, surfaceNoUpdate: false });

  const intervalMs = intervalMsFor(cadence);
  if (intervalMs !== null) {
    updateTimer = setInterval(() => {
      void runUpdateCheck(ctx, { force: false, surfaceNoUpdate: false });
    }, intervalMs);
  }
}

function rescheduleUpdateCheck(ctx: ExtensionContext) {
  disposeUpdateTimer();
  scheduleUpdateCheck(ctx);
}

/**
 * One pass of the update check: throttle gate → HEAD probe →
 * version compare → source-aware prompt. Silent on the no-op paths
 * unless `surfaceNoUpdate` is set (the explicit
 * `greycat.checkForUpdates` command sets it so the user gets feedback).
 */
async function runUpdateCheck(
  ctx: ExtensionContext,
  opts: { force: boolean; surfaceNoUpdate: boolean },
): Promise<void> {
  const cadence = readCheckCadence();
  if (cadence === 'off' && !opts.force) {
    return;
  }
  const intervalMs = intervalMsFor(cadence);
  if (!opts.force && intervalMs !== null) {
    const last = ctx.globalState.get<number>(LAST_UPDATE_CHECK_KEY);
    if (typeof last === 'number' && Date.now() - last < intervalMs) {
      return;
    }
  }
  try {
    const tag = await probeLatestTag();
    await ctx.globalState.update(LAST_UPDATE_CHECK_KEY, Date.now());
    if (!tag) {
      channel.appendLine('[update-check] could not resolve latest tag');
      return;
    }
    latestKnownTag = tag;
    refreshStatusBar();

    const current = currentInstalledTag();
    if (!current) {
      channel.appendLine(`[update-check] latest=${tag} current=(unknown)`);
      if (opts.surfaceNoUpdate) {
        window.showInformationMessage(
          `GreyCat: latest release is ${tag} (couldn't determine your installed version).`,
        );
      }
      return;
    }
    const cmp = semverCompare(stripV(tag), stripV(current));
    channel.appendLine(`[update-check] latest=${tag} current=${current} cmp=${cmp}`);
    if (cmp <= 0) {
      if (opts.surfaceNoUpdate) {
        window.showInformationMessage(`GreyCat: analyzer is up to date (${current}).`);
      }
      return;
    }

    const skipped = ctx.globalState.get<string>(SKIPPED_VERSION_KEY);
    if (!opts.force && skipped === tag) {
      channel.appendLine(`[update-check] skipped tag ${tag} per user choice`);
      return;
    }

    await promptUpdate(ctx, tag, current);
  } catch (err) {
    const message = err instanceof Error ? err.message : String(err);
    channel.appendLine(`[update-check] failed: ${message}`);
  }
}

function currentInstalledTag(): string | null {
  if (!resolvedBinary || !resolvedBinary.version) {
    return null;
  }
  // `--version` prints e.g. `greycat-analyzer 0.1.4` — pick the first
  // token that looks like a semver tail.
  const match = resolvedBinary.version.match(/(\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?)/);
  return match ? `v${match[1]}` : null;
}

/**
 * Surface the update notification. Shape depends on where the current
 * binary came from: managed cache → offers an in-extension **Update**
 * button; PATH / settings → only opens release notes (we don't own
 * that binary).
 */
async function promptUpdate(ctx: ExtensionContext, tag: string, current: string) {
  const releaseNotesUrl = `https://github.com/${REPO}/releases/tag/${tag}`;
  if (!resolvedBinary) {
    return;
  }
  if (resolvedBinary.source === 'managed') {
    const choice = await window.showInformationMessage(
      `GreyCat analyzer ${tag} is available (you have ${current}).`,
      'Update',
      'Release notes',
      'Later',
      'Skip this version',
    );
    if (choice === 'Update') {
      await downloadServerCmd(ctx);
    } else if (choice === 'Release notes') {
      void env.openExternal(Uri.parse(releaseNotesUrl));
    } else if (choice === 'Skip this version') {
      await ctx.globalState.update(SKIPPED_VERSION_KEY, tag);
    }
  } else {
    const where =
      resolvedBinary.source === 'settings' ? '`greycat.serverPath`' : `PATH at \`${resolvedBinary.command}\``;
    const choice = await window.showInformationMessage(
      `GreyCat analyzer ${tag} is available. You're running ${current} from ${where} — update via the channel you installed it.`,
      'Release notes',
      'Later',
      'Skip this version',
    );
    if (choice === 'Release notes') {
      void env.openExternal(Uri.parse(releaseNotesUrl));
    } else if (choice === 'Skip this version') {
      await ctx.globalState.update(SKIPPED_VERSION_KEY, tag);
    }
  }
}

/**
 * `HEAD /releases/latest` and read the `Location` header. The redirect
 * lands at `/releases/tag/vX.Y.Z`; we extract the tag suffix. No GitHub
 * API quota is spent.
 */
async function probeLatestTag(): Promise<string | null> {
  const loc = await resolveRedirect(LATEST_RELEASE_URL);
  if (!loc) {
    return null;
  }
  const match = loc.match(/\/releases\/tag\/([^/?#]+)/);
  return match ? decodeURIComponent(match[1]) : null;
}

function stripV(tag: string): string {
  return tag.startsWith('v') ? tag.slice(1) : tag;
}

/**
 * Returns negative when `a < b`, positive when `a > b`, 0 when equal.
 * Strict X.Y.Z(-pre)? parser. Falls back to a string compare when
 * either side is malformed; that means we'll never *miss* a real
 * update just because the format is unusual, even if the ordering is
 * lexical instead of numeric.
 */
function semverCompare(a: string, b: string): number {
  const pa = parseSemver(a);
  const pb = parseSemver(b);
  if (!pa || !pb) {
    return a < b ? -1 : a > b ? 1 : 0;
  }
  for (let i = 0; i < 3; i++) {
    if (pa.parts[i] !== pb.parts[i]) {
      return pa.parts[i] - pb.parts[i];
    }
  }
  // Pre-release ordering: a non-pre version is > any pre-release of
  // the same X.Y.Z (matches semver spec).
  if (pa.pre && !pb.pre) {
    return -1;
  }
  if (!pa.pre && pb.pre) {
    return 1;
  }
  if (pa.pre && pb.pre) {
    return pa.pre < pb.pre ? -1 : pa.pre > pb.pre ? 1 : 0;
  }
  return 0;
}

function parseSemver(v: string): { parts: [number, number, number]; pre: string | null } | null {
  const m = v.match(/^(\d+)\.(\d+)\.(\d+)(?:-([0-9A-Za-z.-]+))?$/);
  if (!m) {
    return null;
  }
  return {
    parts: [parseInt(m[1], 10), parseInt(m[2], 10), parseInt(m[3], 10)],
    pre: m[4] ?? null,
  };
}

