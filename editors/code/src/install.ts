import {
  chmodSync,
  createWriteStream,
  existsSync,
  mkdirSync,
  rmSync,
  readFileSync,
  unlinkSync,
} from 'fs';
import { tmpdir } from 'os';
import * as path from 'path';
import { spawnSync } from 'child_process';
import { createHash } from 'crypto';
import * as https from 'https';
import { URL } from 'url';

import AdmZip = require('adm-zip');
import {
  CancellationToken,
  CancellationTokenSource,
  ExtensionContext,
  OutputChannel,
  ProgressLocation,
  window,
} from 'vscode';

const RELEASE_BASE = 'https://github.com/maxleiko/greycat-analyzer/releases/latest/download';

/**
 * Per-platform target triple → release-asset filename. The platform tag
 * (`win32` / `darwin` / `linux`) and arch (`x64` / `arm64`) come from
 * Node's `process.platform` + `process.arch`. The single combo we know
 * about but don't ship today is `darwin` / `x64` (Intel Mac) — that
 * surfaces as an actionable error so the user installs manually.
 */
type Target = {
  /** Rust target triple, embedded in the asset name. */
  triple: string;
  /** Binary filename inside the zip. */
  binaryName: string;
};

function detectTarget(): Target | null {
  if (process.platform === 'linux' && process.arch === 'x64') {
    return { triple: 'x86_64-unknown-linux-gnu', binaryName: 'greycat-analyzer' };
  }
  if (process.platform === 'darwin' && process.arch === 'arm64') {
    return { triple: 'aarch64-apple-darwin', binaryName: 'greycat-analyzer' };
  }
  if (process.platform === 'win32' && process.arch === 'x64') {
    return { triple: 'x86_64-pc-windows-msvc', binaryName: 'greycat-analyzer.exe' };
  }
  return null;
}

export function assetUrlFor(target: Target): string {
  return `${RELEASE_BASE}/greycat-analyzer-${target.triple}.zip`;
}

export function sha256SumsUrl(): string {
  return `${RELEASE_BASE}/SHA256SUMS`;
}

/**
 * Path to the managed binary inside the extension's global storage,
 * regardless of whether the binary exists yet.
 */
export function managedBinaryPath(ctx: ExtensionContext): string | null {
  const target = detectTarget();
  if (!target) {
    return null;
  }
  return path.join(ctx.globalStorageUri.fsPath, target.binaryName);
}

/**
 * Probe `<bin> --version` and return the trimmed stdout if the binary
 * exits 0, else `null`. Used both to validate a candidate binary
 * (during discovery) and to capture the version string for the
 * status-bar / update-check.
 */
export function probeVersion(binPath: string): string | null {
  try {
    const out = spawnSync(binPath, ['--version'], { encoding: 'utf8', timeout: 5_000 });
    if (out.status !== 0) {
      return null;
    }
    return (out.stdout || '').trim() || null;
  } catch {
    return null;
  }
}

/**
 * Public entry point — drives the full install: detect platform, fetch
 * the asset under a progress notification, verify the SHA-256 against
 * the published `SHA256SUMS`, unzip into the extension's global
 * storage, mark the binary executable, and return the absolute path.
 *
 * Throws on any failure (caller surfaces as an error notification).
 */
export async function downloadAndInstall(
  ctx: ExtensionContext,
  channel: OutputChannel,
): Promise<{ binaryPath: string; version: string }> {
  const target = detectTarget();
  if (!target) {
    const detail = `platform=${process.platform} arch=${process.arch}`;
    throw new Error(
      `No prebuilt greycat-analyzer artifact for this system (${detail}). Install manually — see https://github.com/maxleiko/greycat-analyzer#install`,
    );
  }
  mkdirSync(ctx.globalStorageUri.fsPath, { recursive: true });
  const zipUrl = assetUrlFor(target);
  const sumsUrl = sha256SumsUrl();

  return await window.withProgress(
    {
      location: ProgressLocation.Notification,
      title: 'GreyCat: downloading analyzer…',
      cancellable: true,
    },
    async (progress, token) => {
      channel.appendLine(`[install] target=${target.triple}`);
      channel.appendLine(`[install] asset=${zipUrl}`);

      const cancel = mergeCancellation(token);

      const tmpZip = path.join(
        tmpdir(),
        `greycat-analyzer-${target.triple}-${Date.now()}.zip`,
      );
      try {
        await downloadToFile(zipUrl, tmpZip, (received, total) => {
          if (total > 0) {
            const pct = Math.round((received / total) * 100);
            progress.report({ message: `${pct}% (${humanBytes(received)} / ${humanBytes(total)})` });
          } else {
            progress.report({ message: humanBytes(received) });
          }
        }, cancel.token);

        progress.report({ message: 'verifying checksum…' });
        const sums = await downloadToString(sumsUrl, cancel.token);
        const expected = sha256For(sums, `greycat-analyzer-${target.triple}.zip`);
        if (!expected) {
          throw new Error(`SHA256SUMS missing entry for greycat-analyzer-${target.triple}.zip`);
        }
        const actual = sha256OfFile(tmpZip);
        if (expected !== actual) {
          throw new Error(`checksum mismatch (expected ${expected}, got ${actual})`);
        }
        channel.appendLine(`[install] sha256 ok (${actual})`);

        progress.report({ message: 'extracting…' });
        const dest = ctx.globalStorageUri.fsPath;
        const zip = new AdmZip(tmpZip);
        zip.extractAllTo(dest, /*overwrite*/ true);
        const binaryPath = path.join(dest, target.binaryName);
        if (!existsSync(binaryPath)) {
          throw new Error(`zip did not contain ${target.binaryName}`);
        }

        if (process.platform !== 'win32') {
          chmodSync(binaryPath, 0o755);
        }
        if (process.platform === 'darwin') {
          // Best effort — `xattr` may not exist (e.g. inside `nix-shell`),
          // or the file may already be unquarantined. Either way, don't
          // block install on it.
          try {
            spawnSync('xattr', ['-d', 'com.apple.quarantine', binaryPath], {
              encoding: 'utf8',
              timeout: 5_000,
            });
          } catch {
            // ignore
          }
        }

        const version = probeVersion(binaryPath);
        if (!version) {
          throw new Error(`installed binary at ${binaryPath} did not respond to --version`);
        }
        channel.appendLine(`[install] installed ${binaryPath} (${version})`);
        return { binaryPath, version };
      } finally {
        try {
          unlinkSync(tmpZip);
        } catch {
          // ignore
        }
      }
    },
  );
}

/**
 * Light wrapper around VS Code's progress cancellation token so the
 * download can be aborted from either side (network failure / user
 * click).
 */
function mergeCancellation(progressToken: CancellationToken): CancellationTokenSource {
  const src = new CancellationTokenSource();
  progressToken.onCancellationRequested(() => src.cancel());
  return src;
}

/**
 * Stream `url` to `dest`, calling `onProgress` with running bytes
 * received and the `Content-Length` total (or `-1` when unknown).
 * Follows up to 5 HTTP redirects; aborts on cancellation.
 */
function downloadToFile(
  url: string,
  dest: string,
  onProgress: (received: number, total: number) => void,
  cancel: CancellationToken,
): Promise<void> {
  return new Promise((resolve, reject) => {
    const file = createWriteStream(dest);
    let received = 0;
    let total = -1;
    let settled = false;
    const settle = (err?: Error) => {
      if (settled) {
        return;
      }
      settled = true;
      file.close();
      if (err) {
        try {
          unlinkSync(dest);
        } catch {
          // ignore
        }
        reject(err);
      } else {
        resolve();
      }
    };
    const onCancel = cancel.onCancellationRequested(() => settle(new Error('download cancelled')));
    requestWithRedirects(
      url,
      (res) => {
        if (res.statusCode && res.statusCode >= 400) {
          settle(new Error(`HTTP ${res.statusCode} fetching ${url}`));
          return;
        }
        const len = parseInt(res.headers['content-length'] || '0', 10);
        if (len > 0) {
          total = len;
        }
        res.on('data', (chunk: Buffer) => {
          received += chunk.length;
          onProgress(received, total);
        });
        res.pipe(file);
        file.on('finish', () => {
          onCancel.dispose();
          settle();
        });
        file.on('error', (err) => {
          onCancel.dispose();
          settle(err);
        });
      },
      (err) => {
        onCancel.dispose();
        settle(err);
      },
    );
  });
}

/**
 * Fetch `url` and resolve with the response body as UTF-8 text. Used
 * for the small `SHA256SUMS` file and the redirect probe; for the
 * binary zip we stream to disk via [`downloadToFile`].
 */
function downloadToString(url: string, cancel: CancellationToken): Promise<string> {
  return new Promise((resolve, reject) => {
    let settled = false;
    const settle = (err?: Error, value?: string) => {
      if (settled) {
        return;
      }
      settled = true;
      if (err) {
        reject(err);
      } else {
        resolve(value || '');
      }
    };
    const onCancel = cancel.onCancellationRequested(() => settle(new Error('download cancelled')));
    requestWithRedirects(
      url,
      (res) => {
        if (res.statusCode && res.statusCode >= 400) {
          settle(new Error(`HTTP ${res.statusCode} fetching ${url}`));
          return;
        }
        const chunks: Buffer[] = [];
        res.on('data', (c: Buffer) => chunks.push(c));
        res.on('end', () => {
          onCancel.dispose();
          settle(undefined, Buffer.concat(chunks).toString('utf8'));
        });
        res.on('error', (err) => {
          onCancel.dispose();
          settle(err);
        });
      },
      (err) => {
        onCancel.dispose();
        settle(err);
      },
    );
  });
}

/**
 * `HEAD` the URL, stop on the first response that's not a redirect,
 * and resolve with the final `Location` header (or `null` when the
 * response is itself terminal). Used by the update-check to read the
 * tag out of `/releases/latest` without spending API quota.
 */
export function resolveRedirect(url: string): Promise<string | null> {
  return new Promise((resolve, reject) => {
    const visit = (current: string, depth: number) => {
      if (depth > 5) {
        return reject(new Error('too many redirects'));
      }
      const u = new URL(current);
      const req = https.request(
        {
          method: 'HEAD',
          hostname: u.hostname,
          path: u.pathname + u.search,
          headers: { 'User-Agent': 'greycat-vscode-extension' },
        },
        (res) => {
          if (res.statusCode && res.statusCode >= 300 && res.statusCode < 400) {
            const loc = res.headers.location;
            if (!loc) {
              resolve(null);
              return;
            }
            const absolute = new URL(loc, current).toString();
            resolve(absolute);
            return;
          }
          resolve(null);
        },
      );
      req.on('error', reject);
      req.end();
    };
    visit(url, 0);
  });
}

/**
 * Issue a `GET` against `url`, follow up to 5 HTTP redirects, then
 * hand the final response to `onResponse`. The `onError` path catches
 * any network-level failure; HTTP-level failures (4xx / 5xx) come
 * through `onResponse` and the caller checks `statusCode`.
 */
function requestWithRedirects(
  url: string,
  onResponse: (res: import('http').IncomingMessage) => void,
  onError: (err: Error) => void,
): void {
  const visit = (current: string, depth: number) => {
    if (depth > 5) {
      onError(new Error('too many redirects'));
      return;
    }
    const u = new URL(current);
    const req = https.get(
      {
        hostname: u.hostname,
        path: u.pathname + u.search,
        headers: { 'User-Agent': 'greycat-vscode-extension' },
      },
      (res) => {
        if (res.statusCode && res.statusCode >= 300 && res.statusCode < 400 && res.headers.location) {
          const next = new URL(res.headers.location, current).toString();
          res.resume();
          visit(next, depth + 1);
          return;
        }
        onResponse(res);
      },
    );
    req.on('error', onError);
  };
  visit(url, 0);
}

/**
 * Parse a `SHA256SUMS` body for `assetName` and return the hex digest
 * if present, otherwise `null`. The file format is the canonical
 * `sha256sum` output: `<64-hex>  <filename>`.
 */
function sha256For(sumsBody: string, assetName: string): string | null {
  for (const line of sumsBody.split(/\r?\n/)) {
    const trimmed = line.trim();
    if (!trimmed) {
      continue;
    }
    const match = trimmed.match(/^([0-9a-fA-F]{64})\s+\*?(.+)$/);
    if (!match) {
      continue;
    }
    const [, hex, name] = match;
    if (name === assetName) {
      return hex.toLowerCase();
    }
  }
  return null;
}

function sha256OfFile(filePath: string): string {
  const hash = createHash('sha256');
  hash.update(readFileSync(filePath));
  return hash.digest('hex');
}

function humanBytes(bytes: number): string {
  if (bytes < 1024) {
    return `${bytes} B`;
  }
  if (bytes < 1024 * 1024) {
    return `${(bytes / 1024).toFixed(1)} KB`;
  }
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}

/**
 * Remove a previously-installed managed binary, if present. Called by
 * the `downloadServer` command before a fresh download so a partial
 * extract from a failed run can't shadow the new install.
 */
export function clearManagedBinary(ctx: ExtensionContext): void {
  const binPath = managedBinaryPath(ctx);
  if (!binPath) {
    return;
  }
  try {
    rmSync(binPath, { force: true });
  } catch {
    // ignore
  }
}
