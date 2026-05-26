import { execSync } from 'child_process';
import { buildSync } from 'esbuild';
import { rmSync } from 'fs';

try {
  // Compile TypeScript
  execSync('tsc', { stdio: 'inherit' });

  // Run esbuild
  buildSync({
    entryPoints: ['src/extension.ts'],
    outfile: 'extension.js',
    bundle: true,
    format: 'cjs',
    platform: 'node',
    target: 'node20',
    minify: true,
    external: ['vscode'],
  });

  // Package the extension with VSCE
  execSync('vsce package -o greycat.vsix', { stdio: 'inherit' });

  // Clean up
  rmSync('extension.js', { force: true });
} catch (err) {
  console.error(err);
  console.error('❌ Build failed');
  process.exit(1);
}
