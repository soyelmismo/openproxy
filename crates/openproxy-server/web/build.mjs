// build.mjs — esbuild bundler for the openproxy dashboard frontend.
//
// Bundles all TS source + lit-html into code-split chunks: a small core bundle (app.js) plus
// lazy-loaded chunks for the heavy views (playground, providers, notifications, analytics,
// config). `rust-embed` picks up everything under `dist/` automatically.

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
  // No sourcemap in production builds — the .map file gets embedded into the Rust binary via
  // rust-embed and wastes RAM. For development debugging, run `node build.mjs --sourcemap`.
  sourcemap: process.argv.includes('--sourcemap'),
  minify: !isWatch,
  legalComments: 'eof',
  packages: 'bundle',
  logLevel: 'info',
};

const stylesDir = join(__dirname, 'src', 'static', 'styles');
const cssOptions = {
  entryPoints: [join(stylesDir, 'index.css')],
  bundle: true,
  outfile: join(outDir, 'app.css'),
  minify: !isWatch,
  legalComments: 'eof',
  external: ['/admin/*'],
  logLevel: 'info',
};

if (isWatch) {
  const [jsCtx, cssCtx] = await Promise.all([
    context(options),
    context(cssOptions),
  ]);
  await Promise.all([jsCtx.watch(), cssCtx.watch()]);
  console.log('Watching for changes...');
} else {
  // Limpiar chunks y bundles viejos para evitar que rust-embed infle el binario
  const { rmSync, mkdirSync } = await import('fs');
  rmSync(outDir, { recursive: true, force: true });
  mkdirSync(outDir, { recursive: true });

  await Promise.all([
    build(options),
    build(cssOptions),
  ]);
  console.log('Build complete: ' + outDir);
}
