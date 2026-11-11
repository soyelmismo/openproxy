// Frontend-only dev sidecar. Proxy API/WS to an already running test backend.
// DEV_HOST=127.0.0.1 DEV_PORT=5173 DEV_BACKEND=http://127.0.0.1:8791 npm run dev
import { context } from 'esbuild';
import http from 'node:http';
import net from 'node:net';
import { readFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';

const root = dirname(fileURLToPath(import.meta.url));
// Bind to the host's CURRENT Tailscale address. It was hardcoded to 127.0.0.1
// and the sidecar died the day Tailscale renumbered the interface: the esbuild
// serve call threw ERR_ADDRESS_NOT_AVAILABLE at startup.
const { execFileSync } = await import('node:child_process');
const tailscaleIp = (() => {
  try { return execFileSync('tailscale', ['ip', '-4'], { encoding: 'utf8' }).trim().split('\n')[0]?.trim(); }
  catch { return null; }
})();
const host = process.env.DEV_HOST || tailscaleIp || '127.0.0.1';
const port = Number(process.env.DEV_PORT || 5173);
const backend = new URL(process.env.DEV_BACKEND || 'http://127.0.0.1:8787');
if (backend.protocol !== 'http:') throw new Error('DEV_BACKEND must use HTTP on a trusted local interface');
const ctx = await context({
  entryPoints: { app: join(root, 'src/static/src/app.ts'), 'theme-init': join(root, 'src/static/src/theme-init.ts'), styles: join(root, 'src/static/styles/index.css') },
  bundle: true, format: 'esm', target: 'es2022', splitting: true,
  outdir: join(root, '.dev-dist'), chunkNames: 'chunks/[name]-[hash]',
  sourcemap: true, external: ['/admin/*'], logLevel: 'info',
});
await ctx.watch();
// esbuild assets are only reached through this process's proxy, so loopback is
// enough; binding the Tailscale address here made every asset request 502 when
// the interface renumbered.
const assets = await ctx.serve({ host: '127.0.0.1', port: 0, servedir: join(root, '.dev-dist') });
const allowedHosts = new Set([`${host}:${port}`, `localhost:${port}`, `127.0.0.1:${port}`]);
const liveReload = `<script type="module">const events = new EventSource('/esbuild'); events.addEventListener('change', () => { if (window === window.top) location.reload(); });</script>`;
function proxy(req, res, target, path = req.url) {
  const upstream = http.request({ hostname: target.hostname, port: target.port, path, method: req.method, headers: { ...req.headers, host: target.host } }, response => {
    res.writeHead(response.statusCode, response.headers);
    response.pipe(res);
  });
  upstream.on('error', () => { if (!res.headersSent) res.writeHead(502); res.end('Development upstream unavailable'); });
  req.on('aborted', () => upstream.destroy());
  res.on('close', () => upstream.destroy());
  req.pipe(upstream);
}
const server = http.createServer(async (req, res) => {
  if (!allowedHosts.has(req.headers.host)) { res.writeHead(403); res.end(); return; }
  const path = new URL(req.url, `http://${req.headers.host}`).pathname;
  // Reject cross-origin writes to the proxied admin API.
  if (req.headers.origin && req.headers.origin !== `http://${req.headers.host}`) { res.writeHead(403); res.end(); return; }
  if (path.startsWith('/admin/api/') || path === '/admin/ws' || path === '/admin/health') { proxy(req, res, backend); return; }
  const panel = path === '/admin/panel' || path === '/admin/panel/';
  res.setHeader('Cache-Control', 'no-store');
  res.setHeader('X-Frame-Options', panel ? 'SAMEORIGIN' : 'DENY');
  res.setHeader('Content-Security-Policy', `frame-ancestors ${panel ? "'self'" : "'none'"}`);
  try {
    if (['/', '/admin', '/admin/', '/admin/panel', '/admin/panel/'].includes(path)) {
      let html = await readFile(join(root, 'src/static/index.html'), 'utf8');
      html = html.replace('/admin/dist/app.css', '/admin/dist/styles.css').replace('</body>', `${liveReload}</body>`);
      res.setHeader('Content-Type', 'text/html; charset=utf-8');
      res.end(html); return;
    }
    if (/^\/admin\/i18n\/[a-zA-Z-]+(?:\.json)?$/.test(path)) {
      res.setHeader('Content-Type', 'application/json');
      res.end(await readFile(join(root, 'src/static/src/i18n', `${path.split('/').pop().replace(/\.json$/, '')}.json`))); return;
    }
    if (path === '/esbuild') { proxy(req, res, new URL(`http://127.0.0.1:${assets.port}`), req.url); return; }
    if (path.startsWith('/admin/dist/')) {
      proxy(req, res, new URL(`http://127.0.0.1:${assets.port}`), req.url.replace('/admin/dist/', '/')); return;
    }
    if (/^\/admin\/(fonts\/[^/]+|callback\.html)$/.test(path)) {
      const file = path.slice('/admin/'.length);
      if (file.split('/').some(part => part === '..' || part === '.')) { res.writeHead(404); res.end(); return; }
      res.setHeader('Content-Type', file.endsWith('.html') ? 'text/html; charset=utf-8' : file.endsWith('.woff2') ? 'font/woff2' : 'application/octet-stream');
      res.end(await readFile(join(root, 'src/static', file))); return;
    }
    res.writeHead(404); res.end();
  } catch { res.writeHead(404); res.end(); }
});
server.on('upgrade', (req, socket, head) => {
  if (!allowedHosts.has(req.headers.host) || new URL(req.url, 'http://dev').pathname !== '/admin/ws' ||
      (req.headers.origin && req.headers.origin !== `http://${req.headers.host}`)) { socket.destroy(); return; }
  const upstream = net.connect(Number(backend.port || 80), backend.hostname, () => {
    upstream.write(`${req.method} ${req.url} HTTP/${req.httpVersion}\r\n`);
    for (let i = 0; i < req.rawHeaders.length; i += 2) upstream.write(`${req.rawHeaders[i]}: ${req.rawHeaders[i + 1]}\r\n`);
    upstream.write('\r\n'); upstream.write(head); socket.pipe(upstream); upstream.pipe(socket);
  });
  upstream.on('error', () => socket.destroy());
  socket.on('error', () => upstream.destroy());
  socket.on('close', () => upstream.destroy());
});
server.listen(port, host, () => console.log(`Frontend sidecar: http://${host}:${port}/admin/\nBackend: ${backend.origin} (existing process, no Rust build)`));
for (const signal of ['SIGINT', 'SIGTERM']) process.on(signal, async () => { server.close(); await ctx.dispose(); process.exit(0); });
