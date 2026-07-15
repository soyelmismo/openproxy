#!/usr/bin/env bash
# scripts/rebuild-and-restart.sh
#
# Build openproxy (release) y reinicia el servicio systemd que lo sirve en
# producción.
#
# Convenciones asumidas (basadas en /etc/systemd/system/openproxy.service):
#   - Proyecto y target/ viven en $PROJECT_DIR (/root/proyectos/openproxy por defecto).
#   - Binario: $PROJECT_DIR/target/release/openproxy.
#   - Servicio systemd: openproxy.
#
# El dashboard SPA es parte del binario openproxy: el frontend (TypeScript +
# lit-html en crates/openproxy-server/web/) se compila con `pnpm build` ANTES
# de `cargo build`, porque `rust-embed` embeda el árbol `dist/` en el binario
# en tiempo de compilación. El server sirve la API (/v1/*, /admin/api/*) y
# el dashboard SPA (/admin/*) en el mismo puerto — no hay binario separado.
#
# Uso:
#   ./scripts/rebuild-and-restart.sh                    # build + restart
#   ./scripts/rebuild-and-restart.sh --no-restart       # solo build
#   ./scripts/rebuild-and-restart.sh --project /path    # override proyecto
#   ./scripts/rebuild-and-restart.sh --features default # override features

set -euo pipefail

PROJECT_DIR="/root/proyectos/openproxy"
FEATURES="--all-features"
DO_RESTART=1
LOG_PREFIX="[rebuild]"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --no-restart) DO_RESTART=0; shift ;;
    --project)    PROJECT_DIR="$2"; shift 2 ;;
    --features)   FEATURES="$2"; shift 2 ;;
    -h|--help)
      sed -n '2,28p' "$0"
      exit 0 ;;
    *)
      echo "$LOG_PREFIX argumento desconocido: $1" >&2
      exit 2 ;;
  esac
done

log() { echo "$LOG_PREFIX $(date +%H:%M:%S) $*"; }
die() { echo "$LOG_PREFIX ERROR: $*" >&2; exit 1; }

[[ -d "$PROJECT_DIR" ]]  || die "PROJECT_DIR no existe: $PROJECT_DIR"
[[ -f "$PROJECT_DIR/Cargo.toml" ]] || die "no hay Cargo.toml en $PROJECT_DIR"

cd "$PROJECT_DIR"
log "pwd=$(pwd)  features=$FEATURES  restart=$DO_RESTART"

# Cabecera: qué vamos a compilar
log "git: $(git rev-parse --short HEAD) on $(git branch --show-current)"
log "working tree: $(git status --porcelain | wc -l) cambios sucios"

# --- Build ---------------------------------------------------------------
# 1) Frontend TS bundle: el frontend vive en
#    crates/openproxy-server/web/ y su `pnpm build` emite a
#    src/static/dist/app.js (esbuild) + .d.ts (tsc --emitDeclarationOnly).
#    El dist/ está en .gitignore, hay que regenerarlo en cada build, ANTES
#    de que cargo compila el binario (rust-embed lo embeda en compile time).
WEB_DIR="$PROJECT_DIR/crates/openproxy-server/web"
if [[ -d "$WEB_DIR" && -f "$WEB_DIR/package.json" ]]; then
  # pnpm puede no estar en el PATH default del script (ej. corre bajo sudo
  # con un PATH limpio). Buscamos en las rutas comunes antes de fallar.
  if ! command -v pnpm >/dev/null 2>&1; then
    for pnpm_dir in /root/.hermes/node/bin /root/.local/share/pnpm /usr/local/bin /usr/bin; do
      if [[ -x "$pnpm_dir/pnpm" ]]; then
        export PATH="$pnpm_dir:$PATH"
        log "pnpm encontrado en $pnpm_dir, agregado al PATH"
        break
      fi
    done
  fi
  if ! command -v pnpm >/dev/null 2>&1; then
    die "pnpm no está en PATH; instalá con 'npm i -g pnpm' o 'corepack enable' (o agregá /root/.hermes/node/bin a PATH) y reintentá"
  fi
  log "pnpm install --frozen-lockfile  (en $WEB_DIR)"
  PNPM_ERR=$(mktemp)
  if ! (cd "$WEB_DIR" && pnpm install --frozen-lockfile) >"$PNPM_ERR" 2>&1; then
    if grep -q "ERR_PNPM_OUTDATED_LOCKFILE" "$PNPM_ERR"; then
      log "⚠️  lockfile desincronizado con package.json — actualizando con --no-frozen-lockfile"
      log "    (committeá el pnpm-lock.yaml actualizado junto al package.json)"
      (cd "$WEB_DIR" && pnpm install --no-frozen-lockfile)
    else
      cat "$PNPM_ERR" >&2
      rm -f "$PNPM_ERR"
      die "pnpm install --frozen-lockfile falló"
    fi
  fi
  rm -f "$PNPM_ERR"
  log "pnpm build  (esbuild emite a $WEB_DIR/src/static/dist/)"
  (cd "$WEB_DIR" && pnpm build)
  # Sanity check: app.js (esbuild bundle) + 2 .d.ts files que demuestran
  # que tsc caminó todo el source tree (esbuild inlinea todo en app.js,
  # así que los .js por módulo no se emiten más).
  for f in dist/app.js dist/lib/escape.d.ts dist/handlers/registry.d.ts; do
    [[ -f "$WEB_DIR/src/static/$f" ]] \
      || die "build no emitió $f en $WEB_DIR/src/static/$f; corré 'cd $WEB_DIR && pnpm build' a mano para ver el error"
  done
  log "pnpm build OK (dist/app.js, lib/escape.d.ts, handlers/registry.d.ts presentes)"
else
  log "saltando frontend TS: $WEB_DIR no existe o no tiene package.json"
fi

# 2) Build del binario. --workspace cubre todo; el servicio sólo arranca
#    openproxy, pero el resto de las crates del workspace debe compilar
#    (core, server, api-client, types, db, compression, adapters, pipeline).
log "cargo build --workspace $FEATURES --release"
cargo build --workspace $FEATURES --release

# Sanity check: el binario que systemctl va a arrancar existe.
BIN_PATH="$PROJECT_DIR/target/release/openproxy"
[[ -x "$BIN_PATH" ]] || die "binario esperado no existe o no es ejecutable: $BIN_PATH"
log "bin OK: $BIN_PATH ($(stat -c %s "$BIN_PATH") bytes, mtime $(stat -c %y "$BIN_PATH" | cut -d. -f1))"

# --- Restart -------------------------------------------------------------
if [[ "$DO_RESTART" -eq 0 ]]; then
  log "--no-restart: servicio NO reiniciado"
  exit 0
fi

if ! command -v systemctl >/dev/null; then
  die "systemctl no disponible; re-ejecuta con --no-restart o expone el servicio manualmente"
fi

log "systemctl daemon-reload"
sudo systemctl daemon-reload

log "systemctl restart openproxy.service"
sudo systemctl restart openproxy.service

# Estado final.
log "--- status final ---"
sudo systemctl --no-pager --full status openproxy.service | sed -n '1,5p' || true

# Activo = running?
svc_active=$(sudo systemctl is-active openproxy.service || true)
log "is-active: openproxy=$svc_active"

[[ "$svc_active" == "active" ]] || die "openproxy no quedó active (=$svc_active). Revisa: journalctl -u openproxy -n 50"

log "OK ✅  openproxy corriendo con la build nueva"
