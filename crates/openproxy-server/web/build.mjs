// build.mjs — esbuild bundler for the openproxy dashboard frontend.
//
// Bundles all TS source + lit-html into code-split chunks: a small
// core bundle (app.js) plus lazy-loaded chunks for heavy views
// (playground, providers, notifications, analytics, config).
// `rust-embed` picks up everything under `dist/` automatically.

import { build, context } from 'esbuild';
import { fileURLToPath } from 'url';
import { dirname, join } from 'path';

const __dirname = dirname(fileURLToPath(import.meta.url));
const isWatch = process.argv.includes('--watch');
const srcDir = join(__dirname, 'src', 'static', 'src');
const outDir = join(__dirname, 'src', 'static', 'dist');

const options = {
  entryPoints: [join(srcDir, 'app.ts')],
  bundle: true,
  format: 'esm',
  target: 'es2022',
  outdir: outDir,
  splitting: true,
  chunkNames: 'chunks/[name]-[hash]',
  // No sourcemap in production builds — the .map file gets embedded
  // into the Rust binary via rust-embed and wastes RAM. For
  // development debugging, run `node build.mjs --sourcemap`.
  sourcemap: process.argv.includes('--sourcemap'),
  minify: !isWatch,
  legalComments: 'eof',
  packages: 'bundle',
  logLevel: 'info',
  // Treat .css imports as plain text strings so the uPlot wrapper can
  // inline the chart CSS via a <style> tag at runtime.
  loader: { '.css': 'text' },
};

if (isWatch) {
  const ctx = await context(options);
  await ctx.watch();
  console.log('Watching for changes...');
} else {
  await build(options);
  console.log('Build complete: ' + outDir);
}
