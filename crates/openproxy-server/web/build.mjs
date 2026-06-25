// build.mjs — esbuild bundler for the openproxy dashboard frontend.
//
// Bundles all TS source + lit-html into a single app.js that the
// browser can load without import maps or a dev server.

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
  outfile: join(outDir, 'app.js'),
  sourcemap: true,
  minify: false,
  legalComments: 'eof',
  packages: 'bundle',
  logLevel: 'info',
  // Treat .css imports as plain text strings so the uPlot wrapper can
  // inline the chart CSS via a <style> tag at runtime. This keeps the
  // bundle as a single app.js (no separate .css output to ship / link).
  loader: { '.css': 'text' },
};

if (isWatch) {
  const ctx = await context(options);
  await ctx.watch();
  console.log('Watching for changes...');
} else {
  await build(options);
  console.log('Build complete: ' + join(outDir, 'app.js'));
}
