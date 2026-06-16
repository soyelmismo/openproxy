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
