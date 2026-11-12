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
  // Chunk-retention deploy: a browser tab opened before this build still holds
  // an app.js that dynamically imports chunks by HASH (chunks/combos-XXXX.js).
  // Deleting those chunks on rebuild makes every lazy route in an open tab die
  // with "Failed to fetch dynamically imported module". So: stash the previous
  // dist, rebuild, then copy back any OLD chunk that the new build did not
  // re-emit (same name = same content). One generation only — older ones die.
  const { rmSync, mkdirSync, renameSync, cpSync, existsSync } = await import('fs');
  const prevDir = outDir + '.prev';
  rmSync(prevDir, { recursive: true, force: true });
  if (existsSync(outDir)) renameSync(outDir, prevDir);
  mkdirSync(outDir, { recursive: true });

  await Promise.all([
    build(options),
    build(cssOptions),
    build(checkoutOptions),
  ]);

  if (existsSync(prevDir)) {
    const prevChunks = join(prevDir, 'chunks');
    const nextChunks = join(outDir, 'chunks');
    if (existsSync(prevChunks)) {
      mkdirSync(nextChunks, { recursive: true });
      cpSync(prevChunks, nextChunks, {
        recursive: true,
        // Copy only the old hashed .js/copy files missing from the new build.
        filter: (src) => {
          const rel = src.slice(prevChunks.length + 1);
          if (!rel) return true;
          return !existsSync(join(nextChunks, rel));
        },
      });
    }
    rmSync(prevDir, { recursive: true, force: true });
  }
  console.log('Build complete: ' + outDir);
}
