#!/usr/bin/env bash
# scripts/rebuild-and-restart.sh
#
# Build openproxy-core y openproxy-web con --all-features (release) y reinicia
# los servicios systemd que los sirven en producción.
#
# Convenciones asumidas (basadas en /etc/systemd/system/openproxy-{core,web}.service):
#   - Proyecto y target/ viven en $PROJECT_DIR (/root/proyectos/openproxy por defecto).
#   - Binarios: $PROJECT_DIR/target/release/{openproxy,openproxy-web}.
#   - Servicios: openproxy-core, openproxy-web.
#
# Uso:
#   ./scripts/rebuild-and-restart.sh                    # build + restart
#   ./scripts/rebuild-and-restart.sh --no-restart       # solo build
#   ./scripts/rebuild-and-restart.sh --project /path    # override proyecto
#   ./scripts/rebuild-and-restod.sh --features default  # override features

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
      sed -n '2,18p' "$0"
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
# 1) Cargo check rápido: si falla, abortamos sin tocar nada.
log "cargo check --workspace $FEATURES --release"
cargo check --workspace $FEATURES --release

# 1.5) Frontend TS bundle: openproxy-web embebe src/static/index.html vía
#      include_str! y sirve el bundle TS desde static/dist/ (commit 19514e1
#      flipeó /src/app.js a /dist/app.js). El dist/ está en .gitignore, hay
#      que regenerarlo en cada build, ANTES de que cargo compila el binario.
WEB_CRATE_DIR="$PROJECT_DIR/crates/openproxy-web"
if [[ -d "$WEB_CRATE_DIR" && -f "$WEB_CRATE_DIR/package.json" ]]; then
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
  log "pnpm install --frozen-lockfile  (en $WEB_CRATE_DIR)"
  (cd "$WEB_CRATE_DIR" && pnpm install --frozen-lockfile)
  log "pnpm build  (tsc emite a $WEB_CRATE_DIR/src/static/dist/)"
  (cd "$WEB_CRATE_DIR" && pnpm build)
  # Sanity check: 3 archivos críticos que el HTML flipado va a pedir.
  for f in dist/app.js dist/lib/escape.js dist/handlers/registry.js; do
    [[ -f "$WEB_CRATE_DIR/src/static/$f" ]] \
      || die "tsc no emitió $f en $WEB_CRATE_DIR/src/static/$f; corré 'cd $WEB_CRATE_DIR && pnpm build' a mano para ver el error"
  done
  log "pnpm build OK (dist/app.js, lib/escape.js, handlers/registry.js presentes)"
else
  log "saltando frontend TS: $WEB_CRATE_DIR no es crate web o no tiene package.json"
fi

# 2) Build de los dos binarios. --workspace cubre todo; los servicios
#    sólo arrancan los dos bins listados, pero el resto del workspace
#    debe compilar (openproxy-server, openproxy-api-client).
log "cargo build --workspace $FEATURES --release"
cargo build --workspace $FEATURES --release

# Sanity check: los dos binarios que systemctl va a arrancar existen.
for bin in openproxy openproxy-web; do
  bin_path="$PROJECT_DIR/target/release/$bin"
  [[ -x "$bin_path" ]] || die "binario esperado no existe o no es ejecutable: $bin_path"
  log "bin OK: $bin_path ($(stat -c %s "$bin_path") bytes, mtime $(stat -c %y "$bin_path" | cut -d. -f1))"
done

# --- Restart -------------------------------------------------------------
if [[ "$DO_RESTART" -eq 0 ]]; then
  log "--no-restart: servicios NO reiniciados"
  exit 0
fi

if ! command -v systemctl >/dev/null; then
  die "systemctl no disponible; re-ejecuta con --no-restart o expone los servicios manualmente"
fi

# Orden: web depende de core (After=openproxy-core.service). Reiniciamos core
# primero y web después; daemon-reload por si cambió algún unit.
log "systemctl daemon-reload"
sudo systemctl daemon-reload

log "systemctl restart openproxy-core.service"
sudo systemctl restart openproxy-core.service

# Pequeña espera: core abre puerto 8787 que web consulta al arrancar. Si web
# arranca antes, su primer healthcheck falla.
for i in 1 2 3 4 5 6 7 8 9 10; do
  if curl -fsS --max-time 1 http://127.0.0.1:8787/healthz >/dev/null 2>&1; then
    log "core listo (healthz OK, intento $i)"
    break
  fi
  sleep 0.5
done

log "systemctl restart openproxy-web.service"
sudo systemctl restart openproxy-web.service

# Estado final.
log "--- status final ---"
sudo systemctl --no-pager --full status openproxy-core.service | sed -n '1,5p' || true
sudo systemctl --no-pager --full status openproxy-web.service  | sed -n '1,5p' || true

# Activo = running?
core_active=$(sudo systemctl is-active openproxy-core.service || true)
web_active=$(sudo systemctl is-active openproxy-web.service  || true)
log "is-active: core=$core_active  web=$web_active"

[[ "$core_active" == "active" ]] || die "openproxy-core no quedó active (=$core_active). Revisa: journalctl -u openproxy-core -n 50"
[[ "$web_active"  == "active" ]] || die "openproxy-web  no quedó active (=$web_active).  Revisa: journalctl -u openproxy-web  -n 50"

log "OK ✅  core y web corriendo con la build nueva"
