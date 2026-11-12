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
  entryPoints: [join(srcDir, 'app.ts'), join(srcDir, 'theme-init.ts')],
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

// Public checkout page: TS source (fragmented under src/checkout/) →
// classic-script IIFE at the path the server serves
// (`include_str!("../web/src/static/checkout.js")`, no-store).
// Lives OUTSIDE dist/ on purpose: the checkout must not ride the SPA's
// hashed-chunk cache dance — it is money surface, always fresh, always
// typechecked like the rest of the dashboard now.
const checkoutOptions = {
  entryPoints: [join(srcDir, 'checkout', 'main.ts')],
  bundle: true,
  format: 'iife',
  target: 'es2022',
  outfile: join(__dirname, 'src', 'static', 'checkout.js'),
  sourcemap: process.argv.includes('--sourcemap'),
  minify: !isWatch,
  legalComments: 'eof',
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
  const [jsCtx, cssCtx, checkoutCtx] = await Promise.all([
    context(options),
    context(cssOptions),
    context(checkoutOptions),
  ]);
  await Promise.all([jsCtx.watch(), cssCtx.watch(), checkoutCtx.watch()]);
  console.log('Watching for changes...');
} else {
  // Limpiar chunks y bundles viejos para evitar que rust-embed infle el binario
  const { rmSync, mkdirSync } = await import('fs');
  rmSync(outDir, { recursive: true, force: true });
  mkdirSync(outDir, { recursive: true });

  await Promise.all([
    build(options),
    build(cssOptions),
    build(checkoutOptions),
  ]);
  console.log('Build complete: ' + outDir);
}
