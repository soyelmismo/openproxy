# REFACTOR_SPEC.md — Frontend Refactor

**Target**: `/crates/openproxy-server/web/src/static/src/`
**Current size**: 30,583 LOC TS + 9,376 LOC CSS (verificado con `find ... | xargs wc -l` en 2026-09-06)
  - `views.css`: 7,259 LOC (1,719 ocurrencias de `!important`)
  - Otros CSS: `tokens.css` 175, `themes.css` 175, `layout.css` 526, `base.css` 118, `components.css` 1,112, `index.css` 11
**Target size**: ~22K LOC TS + ~6K LOC CSS (~28% reducción)
**Bundle target**: < 500KB minified (actual: esbuild single-bundle, no minificado)

> **Nota de auditoría**: Este spec se redacta con base en análisis directo del codebase (no hay informes previos en el repo — `find ... -name "*REPORT*" -o -name "*ANALYSIS*"` retorna 0 resultados). Cada hallazgo incluye el comando `grep`/`wc` que lo verifica. Fecha de referencia: 2026-09-06.

---

## 1. Resumen Ejecutivo

### Objetivos
1. Eliminar 11 componentes sin importers + exports muertos en `charts.ts` (~770 LOC estimados).
2. Deduplicar ~1200 LOC de lógica repetida (clipboard, modal, card-table, refetch, formatContext, alert/confirm).
3. Romper 4 monolitos (`playground.ts` 3,381L, `log-detail.ts` 2,004L, `providers.ts` 1,481L, `notifications.ts` 1,330L) en módulos de < 600 LOC.
4. Reducir burocracia para añadir vistas (de 5-11 archivos a 1 archivo + 1 import).
5. Introducir code-splitting lazy-loading para playground (deferred a Fase 4 — ver Q16).

### Metricas de exito

| Metrica | Baseline (verificado) | Meta | Comando de verificacion |
|---|---|---|---|
| LOC totales TS | 30,583 | < 22,000 | `find crates/openproxy-server/web/src -name "*.ts" -not -path "*/node_modules/*" \| xargs wc -l \| tail -1` |
| LOC totales CSS | 9,376 | < 6,000 | `find crates/openproxy-server/web/src/static/styles -name "*.css" \| xargs wc -l \| tail -1` |
| `!important` en `views.css` | 1,719 | < 500 (F1-F3), < 200 (F5) | `grep -c "!important" crates/openproxy-server/web/src/static/styles/views.css` |
| Bundle minified | N/A (no minificado) | < 500 KB | `node build.mjs && ls -lh src/static/dist/app.js` |
| Archivos > 800 LOC | 4 (playground 3381, log-detail 2004, providers 1481, notifications 1330) | 0 | `wc -l views/*.ts components/**/*.ts handlers/*.ts \| awk '$1>800'` |
| `alert()/confirm()/prompt()` nativos | 49 (10 `alert` + 33 `confirm` + 6 `prompt` en handlers/views) | 0 | `grep -rEn "\b(alert\|confirm\|prompt)\(" crates/openproxy-server/web/src/static/src/views crates/openproxy-server/web/src/static/src/handlers crates/openproxy-server/web/src/static/src/components \| grep -v "\.test\."` |
| Componentes sin importers | 11 (`badge`, `banner`, `bulk-actions`, `card`, `chip`, `detail-tabs`, `filter-bar`, `page-header`, `phase-cell`, `quota-bar`, `table`) | 0 | `for f in components/*.ts; do bn=$(basename "$f" .ts); n=$(grep -rln "from.*components/${bn}\b" src/ \| wc -l); echo "$bn: $n"; done` |
| Breakpoints declarados (CSS) | 7 (480, 639, 768, 900, 1023, 1080, 1100) | 4 (640, 768, 1024, 1280) | `grep -h "@media" src/static/styles/views.css \| sort -u` |
| Tests unitarios | 5 archivos (`format.test.ts`, `ws.test.ts`, `auth.test.ts`, `notifications-store.test.ts`, `live-logs-store.test.ts`) | >= 40% coverage en `lib/`/`handlers/`/`components/` | `find src -name "*.test.ts" \| wc -l` |
| Tests e2e (Playwright) | 17 specs | >= 17 (no regresion) | `find tests/e2e -name "*.spec.ts" \| wc -l` |
| Lighthouse mobile (Perf, A11y) | Sin baseline | >= 90 | Lighthouse CLI |

### No-objetivos
- No se reescribe el sistema de estado existente.
- No se cambia la arquitectura backend ni las rutas REST.
- No se cambia lit-html por otro framework (ni se introduce LitElement — el proyecto solo usa render functions; verificado: `grep -rln "customElements.define\|extends LitElement" src/` retorna 0 resultados no-test).
- No se implementa i18n completa (solo extracción de strings hardcoded adicionales al `en.json` existente).
- No se migra a uPlot completa (`charts.ts`).
- No se elimina la deuda técnica de markdown render (`playground`).
- No se añaden visual regression tests.

---

## 2. Arquitectura Objetivo

### Arquitectura de imports (verificada 2026-09-06)

**Patrón actual**: el proyecto usa **lit-html render functions**, NO Web Components / LitElement.
- `grep -rln "customElements.define\|extends LitElement" src/` → 0 resultados no-test (única referencia: `views/login.ts` que tiene un comentario incidental).
- El componente `modal.ts` ya es una render function: `export function modal(props: ModalProps): TemplateResult`.

**Dirección de dependencias actual** (verificada con `head ... | grep import`):
- `lib/api.ts` importa de `state/auth.ts` y `state/index.ts` — esta violación de la regla "lib/ ← state" existe y es funcional. **Q15 debe respetar esta dependencia existente**; refactorizarla está fuera del scope de este spec. (Q16 fue movido a Fase 4.)

```
app.ts (entry point)
  -> routes (router.ts, lazy-loaded views)
       -> views/* (orchestrators de pagina)
            -> components/* (widgets reusables como render functions)
            -> handlers/* (event logic, DOM manipulation)
            -> lib/* (pure functions, utilities)
            -> state/* (reactive state management)
            -> i18n/* (string lookup)
```

**Reglas de dependencia** (objetivo, no se modifica lo existente en este spec):
- `lib/` no importa de `components/`, `views/` ni `handlers/`. (Excepción documentada: `lib/api.ts` → `state/`, pre-existente.)
- `components/` importa de `lib/` y `state/`. No importa de `views/` ni `handlers/`.
- `handlers/` importa de `lib/`, `state/`, `components/` y `types/`. No importa de `views/`.
- `views/` importa de `handlers/`, `components/`, `lib/`, `state/`, `types/`.

### Nuevos modulos a crear

| Modulo | Responsabilidad unica | LOC estimados | Patron |
|---|---|---|---|
| `lib/clipboard.ts` | `copyToClipboard()` centralizado | ~25 | Pure function |
| `lib/mutate.ts` | `mutateAndRefresh()` post-mutation refetch | ~35 | Pure function + callback |
| `lib/visibility-interval.ts` | `suppressMissedTicks()` pausable (ver S-1) | ~40 | Lifecycle wrapper |
| `lib/show-confirm.ts` | `showConfirm()` / `showPrompt()` (reemplazo nativo) | ~80 | Render function + Promise |
| `components/render-op-modal.ts` | `renderOpModal(props)` reusable modal | ~120 | Render function (NO Web Component) |
| `components/render-responsive-card-table.ts` | `renderResponsiveCardTable(props)` | ~200 | Render function (NO Web Component) |
| `lib/view-factory.ts` | `createView()` scaffolding generico | ~80 | LitElement decorator (única excepción justificada) |
| `lib/router.ts` (modificado) | Lazy-loading de views via dynamic import | ~30 cambios | Modificar existente |

> **Justificación del cambio `<op-modal>` → `renderOpModal()`**: El spec original proponía Web Components. El codebase usa lit-html render functions + `render(template, container)` del package `lit-html`. `modal.ts` ya implementa el patrón correcto (`export function modal(props): TemplateResult`). Forzar LitElement introduciría una inconsistencia arquitectural y duplicaría el bundle (~30KB LitElement base class).

### Convenciones

- **Naming**: camelCase para funciones/variables, PascalCase para clases LitElement (caso `createView()`), kebab-case para nombres de archivo TS.
- **Exports**: Named exports siempre. Default export solo en `app.ts` (entry point).
- **Render functions**: Componentes UI retornan `TemplateResult` (NO Web Components). Uso: `render(template, container)` del paquete `lit-html`. Ejemplo existente: `modal(props)` en `components/modal.ts`.
- **Prohibido**: `alert()`, `confirm()`, `prompt()`, `console.log()` en produccion, `any` tipo, `// TODO` sin issue.
- **Prefijo `render-`** para nombres de archivos que exportan render functions (`render-op-modal.ts`, `render-responsive-card-table.ts`).

---

## 3. Inventario de Cambios por Fase

### FASE 1 — Limpieza Estructural (3 PRs)

#### Q1: Eliminar 11 componentes dead-code

| Accion | Archivos |
|---|---|
| ELIMINAR | `components/badge.ts`, `components/banner.ts`, `components/bulk-actions.ts`, `components/card.ts`, `components/chip.ts`, `components/detail-tabs.ts`, `components/filter-bar.ts`, `components/page-header.ts`, `components/phase-cell.ts`, `components/quota-bar.ts`, `components/table.ts` |

**Verificación de importers = 0** (comando: `for f in components/*.ts; do bn=$(basename "$f" .ts); n=$(grep -rln "from.*components/${bn}\b" src/ | wc -l); echo "$bn: $n"; done`):
- `badge: 0`, `banner: 0`, `bulk-actions: 0`, `card: 0`, `chip: 0`, `detail-tabs: 0`, `filter-bar: 0`, `page-header: 0`, `phase-cell: 0`, `quota-bar: 0`, `table: 0` → **11 archivos con 0 importers**.

**Criterio de aceptacion**:
- `grep -r "from.*components/(badge|banner|bulk-actions|card|chip|detail-tabs|filter-bar|page-header|phase-cell|quota-bar|table)" src/` retorna 0 matches.
- `pnpm typecheck` pasa sin errores.
- `pnpm build` completa sin errores.
- `ls src/static/src/components/modal.ts` NO se elimina (es render function activa con 0 importers directos pero `views/keys.ts` y otros la invocan via `modal()`).

**Impacto**: ~770 LOC estimados eliminados.

**Riesgo**: Nivel bajo. Mitigar con `cargo clippy` + `pnpm build` post-eliminacion.

#### Q2: Activar minify en build

| Accion | Archivos |
|---|---|
| MODIFICAR | `build.mjs` (cambiar `minify: false` a `minify: !isWatch`) |

**Criterio de aceptacion**:
- `node build.mjs` produce archivo < 500KB.
- `node build.mjs --sourcemap` sigue funcionando.
- Dashboard funciona correctamente en browser tras build minificado.

**Impacto**: Bundle ~886KB -> estimado ~380KB minified.

**Riesgo**: Medio. El minify puede romper templates lit-html con string interpolation compleja. Mitigar verificando todas las vistas manualmente tras build.

#### Q3: Limpiar artefactos de test

| Accion | Archivos |
|---|---|
| ELIMINAR | `playwright-state.json` |
| ELIMINAR | `check_console.mjs` |
| MODIFICAR | `playwright.config.js` (eliminar referencia a check_console si existe) |

**Criterio de aceptacion**:
- `playwright-state.json` no existe en repo.
- `pnpm build` no falla.
- Ningun script en package.json referencia `check_console`.

**Impacto**: ~100 LOC eliminados.

**Riesgo**: Bajo. Solo limpieza de archivos de test no usados.

#### Q4: Centralizar clipboard copy

| Accion | Archivos |
|---|---|
| CREAR | `lib/clipboard.ts` (~25 LOC) |
| MODIFICAR | `views/proxies.ts`, `views/providers.ts`, `views/keys.ts`, `views/debug-logs.ts`, `views/playground.ts` (remover duplicados, importar de lib/clipboard) |

**Criterio de aceptacion**:
- `lib/clipboard.ts` exporta `copyToClipboard(text: string): Promise<void>`.
- `grep -c "navigator.clipboard" views/*.ts` suma 0 (todo centralizado en lib/).
- `pnpm typecheck` pasa.
- Funciona en: HTTPS, HTTP (fallback a execCommand), sin clipboard API.

**Impacto**: ~120 LOC eliminados (9 implementaciones -> 1).

**Riesgo**: Bajo. La API de clipboard es simple. Fallback a `document.execCommand('copy')` para HTTP.

#### Q5: Reemplazar alert()/confirm()/prompt()

| Accion | Archivos |
|---|---|
| MODIFICAR | 3 archivos con `alert()` (10 ocurrencias) → `showApiError()` |
| MODIFICAR | 8 archivos con `confirm()` (33 ocurrencias) → `showConfirm()` |
| MODIFICAR | 3 archivos con `prompt()` (6 ocurrencias) → `showPrompt()` |
| CREAR | `lib/show-confirm.ts` (~80 LOC) — render functions que retornan `Promise<boolean>` / `Promise<string\|null>` |

**Inventario verificado (2026-09-06)** — comando: `grep -rEn "\b(alert|confirm|prompt)\(" src/static/src/views src/static/src/handlers src/static/src/components | grep -v "\.test\."`:

- `alert()` (10 sitios):
  - `handlers/key-handlers.ts:235, 342, 359, 371, 384, 401` (6 ocurrencias, todas `alert("Error: " + msg)`)
  - `handlers/combo-handlers.ts:241`
  - `handlers/provider-handlers.ts:228, 251`
  - (2 sitios adicionales en `views/` — verificar con grep)
- `confirm()` (33 sitios):
  - `views/providers.ts:203, 267, 292, 382, 438, 630, 654, 701, 814` (9)
  - `views/keys.ts:66, 78, 91` (3)
  - `views/combos.ts:121, 141` (2)
  - `handlers/model-handlers.ts:203, 347, 396, 438` (4)
  - `handlers/key-handlers.ts:365, 377, 394` (3)
  - `handlers/combo-target-handlers.ts:845, 956` (2)
  - `handlers/combo-handlers.ts:225` (1)
  - `handlers/account-handlers.ts:104` (1)
  - `handlers/proxy-source-handlers.ts:237` (1)
  - `handlers/provider-handlers.ts:208, 232, 266, 306, 720, 828` (6)
  - `handlers/proxy-handlers.ts:117` (1)
- `prompt()` (6 sitios):
  - `views/providers.ts:193, 287`
  - `handlers/account-handlers.ts:187, 223`
  - `handlers/provider-handlers.ts:225, 291`

**Total: 49 sitios** (coincide con `grep -rEn "\b(alert|confirm|prompt)\(" src/static/src/{views,handlers,components} | grep -v "\.test\." | wc -l`).

**Criterio de aceptacion**:
- `grep -rEn "^\s*(alert|confirm|prompt)\(" views/ handlers/ components/` retorna 0 matches.
- Cada acción que antes usaba `confirm()` ahora muestra un modal con título, mensaje y botones OK/Cancelar, retornando `Promise<boolean>`.
- Cada acción que antes usaba `prompt()` ahora muestra un modal con input de texto, retornando `Promise<string|null>`.
- `pnpm typecheck` pasa.
- `pnpm build` pasa.

**Impacto**: ~49 sitios de código reescritos (no eliminados, sino reescritos con Promise-based modal calls). ~80 LOC nuevos en `lib/show-confirm.ts`.

**Riesgo**: Alto. 16 acciones son críticas (regenerate key, delete proxy, etc.). Mitigar: mapear cada sitio con el flujo original, testear cada uno manualmente. PR dedicado.

#### Q6: Tipar state.apiKeys, eliminar tipos muertos

| Accion | Archivos |
|---|---|
| MODIFICAR | `state/` (anadir tipo ApiKey si falta) |
| MODIFICAR | `lib/types/` (eliminar tipos no referenciados) |

**Criterio de aceptacion**:
- `grep -rn "any" lib/types/ | wc -l` < baseline actual.
- `pnpm typecheck` sin errores de tipo.
- `tsconfig.json` puede tener `"strict": true` sin errores新增.

**Impacto**: Variable. Estimado ~50 LOC.

**Riesgo**: Bajo-Medio. Los tipos muertos pueden tener dependencias transitivas. Mitigar con verificacion incremental de typecheck.

#### Q7: Crear `renderOpModal()` reutilizable (render function, NO Web Component)

| Accion | Archivos |
|---|---|
| CREAR | `components/render-op-modal.ts` (~120 LOC) — `renderOpModal(props): TemplateResult` |
| MODIFICAR | Handlers/views que usaran el modal (Q5 + Q14) |

**Decisión arquitectónica**: El spec original proponía `<op-modal>` como Web Component (custom element). **Esto es incorrecto** para este codebase:
- `grep -rln "customElements.define\|extends LitElement" src/` retorna 0 resultados no-test.
- `components/modal.ts` ya implementa exactamente el patrón correcto: `export function modal(props: ModalProps): TemplateResult`.
- `views/keys.ts`, `views/providers.ts`, etc. consumen `modal()` directamente como TemplateResult.
- Forzar LitElement introduciría ~30KB extra al bundle y duplicaría el patrón existente.

**API objetivo** (consistente con `modal()` existente pero ampliada):
```ts
export interface OpModalProps {
  title: string;
  body: string;            // HTML raw — wrap via unsafeHTML internamente
  actions: ModalAction[];  // botones de accion
  danger?: boolean;        // estilo del boton principal
  onClose?: () => void;    // callback al cerrar (Escape, backdrop, X)
}

export function renderOpModal(props: OpModalProps): TemplateResult;
```

**Acceso desde handlers** (ver S-5): El handler crea un container temporal, monta el modal via `render(template, container)`, lo añade al DOM, y retorna una `Promise` que se resuelve cuando el usuario hace click en una acción o cierra. Ejemplo:
```ts
const result = await showModalOp({
  title: "Regenerate Key",
  body: "<p>This will invalidate the current key.</p>",
  actions: [{ label: "Cancel", variant: "secondary" }, { label: "Regenerate", variant: "danger" }],
});
if (result === "Regenerate") { /* doRegenerate(); */ }
```

**Criterio de aceptacion**:
- `components/render-op-modal.ts` exporta `renderOpModal(props): TemplateResult`.
- Focus trap funciona (Tab no escapa del modal).
- Escape cierra el modal.
- `aria-modal="true"` y `role="dialog"` presentes.
- Backdrop click cierra el modal.
- Animación `prefers-reduced-motion` respeta.
- `pnpm typecheck` pasa.
- 1 test unitario mínimo para focus trap.

**Impacto**: ~120 LOC nuevos. Reemplaza scaffolding manual en ~5 sites.

**Riesgo**: Medio. El focus trap es propenso a bugs. Mitigar con test unitario + verificación manual con keyboard navigation.

#### Q8: Unificar breakpoints a 4

| Accion | Archivos |
|---|---|
| MODIFICAR | `src/static/styles/views.css` (reemplazar 7 breakpoints) |
| MODIFICAR | Cualquier view/component con media queries inline |

**Inventario verificado** (comando: `grep -rh "@media" src/static/styles/views.css | sort -u`):
- `@media (max-width: 480px)` (1)
- `@media (max-width: 639px)` (1)
- `@media (max-width: 768px)` (1)
- `@media (max-width: 900px)` (1)
- `@media (max-width: 1023px) and (min-width: 769px)` (1) — compuesto
- `@media (max-width: 1080px)` (1)
- `@media (max-width: 1100px)` (1)
- `@media (min-width: 769px)` (1) — outlier con min-width

**Tabla de mapeo**: Ver seccion 5.

**Criterio de aceptacion**:
- `grep -c "@media" src/static/styles/views.css` <= 4 valores de breakpoint (640, 768, 1024, 1280) en toda la app.
- No existen breakpoints 480, 639, 900, 1023, 1080, 1100 en ningún archivo.
- Dashboard funciona correctamente en 375px (mobile), 768px (tablet), 1024px (laptop), 1440px (desktop).
- La reducción de `!important` queda registrada en métricas — este Q no ataca `!important` directamente, es prerrequisito para Q9.

**Impacto**: ~100 LOC CSS modificados.

**Riesgo**: Medio-Alto. Los breakpoints son visuales; cualquier cambio afecta el layout. Mitigar: screenshot comparison en 4 viewports después de cada breakpoint migration.

#### Q9: Migrar tokens CSS

| Accion | Archivos |
|---|---|
| MODIFICAR | `src/static/styles/views.css` (reemplazar 346 `font-size:` hardcoded — comando: `grep -c "font-size" src/static/styles/views.css`) |
| MODIFICAR | Componentes con inline styles hardcoded |

**Tokens existentes (verificados en `tokens.css`, 2026-09-06)**:
- ✅ `--fs-xs/sm/md/lg/xl/2xl/3xl` (0.72/0.85/0.95/1.1/1.3/1.7/2.1 rem)
- ✅ `--radius-sm/md/lg` (4px/6px/10px), `--radius-0`, `--radius-pill`
- ✅ `--shadow-sm` y `--shadow-md` (faltante: `--shadow-lg` y `--shadow-xl`)
- ✅ `--color-primary` ES Dell red (`var(--c-dell-red)` = #e91d2a). **No hay fallback indigo**.
- ✅ `--color-primary-hover` (#c91825), `--color-primary-fg`, `--color-primary-soft`

**Tokens a crear (solo los realmente faltantes)**:

| Token | Valor | Razon |
|---|---|---|
| `--shadow-lg` | `0 20px 40px rgba(16, 24, 40, 0.12)` | Única shadow que falta |
| `--shadow-xl` | `0 30px 60px rgba(16, 24, 40, 0.16)` | Usado en algunos modales |

**NO se crea**: `--font-size-*` (ya existen como `--fs-*`), `--radius-*` (ya existen), `--color-primary` (ya correcto).

**Criterio de aceptacion**:
- `grep -c "!important" src/static/styles/views.css` < 500 (meta realista — baseline = 1,719).
- `--shadow-lg` y `--shadow-xl` están definidos en `tokens.css` y son consumidos via `var(--shadow-lg)`.
- `views.css` no contiene `font-size:` hardcoded fuera de tokens (todos vía `var(--fs-*)`).
- Visual regression: todas las vistas se ven idénticas al baseline.

**Impacto**: ~150 LOC CSS modificados (font-sizes) + 8 LOC nuevos en tokens.

**Riesgo**: Medio. Los tokens afectan toda la app. Mitigar: cambiar de a un token a la vez, verificar visualmente.

---

### FASE 2 — Datos y Tablas (2 PRs)

#### Q10: Crear `lib/mutate.ts`

**Estado real verificado (2026-09-06)**: el patrón NO es idéntico en 26 sitios. La realidad es:
- 49 ocurrencias de `requestUpdate()` en **8 archivos** de `handlers/`.
- Distribución: `provider-handlers.ts` (~15), `key-handlers.ts` (~10), `combo-handlers.ts` (~6), `model-handlers.ts` (~5), `proxy-handlers.ts` (~4), `account-handlers.ts` (~3), `combo-target-handlers.ts` (~3), `proxy-source-handlers.ts` (~3).
- Patrón general: `try { await api(); toast.success(); requestUpdate(); } catch (e) { showApiError(e, msg); }`.
- **Variaciones reales** que el helper debe acomodar:
  - Algunos handlers llaman `requestUpdate()` múltiples veces (e.g. update local state + refresh).
  - Algunos tienen side-effects extra: navegación, reset de formularios, refresh de stores (`state.refresh()`).
  - El toast de éxito es opcional en algunos sitios (operaciones silenciosas como toggle).
  - Algunos handlers usan `Promise.allSettled([api, api2])` para múltiples llamadas.

| Accion | Archivos |
|---|---|
| CREAR | `lib/mutate.ts` (~35 LOC) |
| MODIFICAR | 8 handlers — caso por caso, NO reemplazo mecánico |

**Especificacion del helper** (ver seccion 4.2): `mutateAndRefresh<T>(options): Promise<boolean>`.

**Estrategia de migracion** (NO bulk-rewrite):
1. **Tier 1 — boilerplate puro** (~25 sitios, 60%): una API call + toast + requestUpdate. Aplicar `mutateAndRefresh` directamente.
2. **Tier 2 — con side-effects simples** (~15 sitios, 30%): API + toast + state.refresh() + requestUpdate(). Usar `mutateAndRefresh` con `onSuccess` callback.
3. **Tier 3 — multi-call** (~5 sitios, 10%): `Promise.allSettled([api1, api2])` + manejo de resultados parciales. Mantener lógica inline; NO forzar al helper.
4. **Tier 4 — flujos críticos** (regenerate key, OAuth flow): mantener inline para legibilidad.

**Criterio de aceptacion**:
- `mutateAndRefresh<T>(options)` centraliza el patrón Tier 1.
- `grep -c "requestUpdate()" handlers/*.ts` reducido en >= 50% (de 49 → <= 24).
- Los handlers Tier 2 usan `onSuccess` callback.
- Los handlers Tier 3/4 quedan explícitamente excluidos con un comentario `// intentionally not using mutateAndRefresh because: <razón>`.
- Errores se manejan uniformemente con `showApiError()`.
- `pnpm typecheck` pasa.
- Cada handler migrado produce el mismo resultado visual que antes.

**Impacto**: ~300 LOC eliminados (boilerplate Tier 1+2 deduplicado).

**Riesgo**: Bajo-Medio. La clasificación Tier 1/2/3/4 requiere lectura caso por caso.

#### Q11: Crear `suppressMissedTicks()` (S-1: renombrar desde `visibilityAwareInterval`)

**Estado real verificado (2026-09-06)**: NO todos los timers son migrables. Inventario (`grep -rn "setInterval\|setTimeout" src/static/src --include="*.ts" | grep -v ".test.ts"`):

| Timer | Ubicación | Tipo | Migrable? | Razon |
|---|---|---|---|---|
| `searchDebounceTimer` | `views/proxies.ts:43, 75` | setTimeout debounce | ❌ NO | Debounce UI, no polling — debe quedarse como setTimeout |
| `searchDebounceTimer` | `views/logs.ts:71, 78` | setTimeout debounce | ❌ NO | Idem |
| Button reset | `views/providers.ts:367, 685, 806` | setTimeout 1.5s | ❌ NO | Reset visual de botón, no polling |
| `vacuumPollHandle` | `views/config.ts:134, 730` | setInterval 5s | ✅ SI | Polling periódico, candidato natural |
| Combo timer | `views/combos.ts:268` | setTimeout | ❌ NO | UI feedback no recurrente |
| `pollHandle` | `views/debug-logs.ts:135, 301` | setTimeout chained | ⚠️ PARCIAL | Comentario en línea 3-4: "chained setTimeout (NOT setInterval — a slow request can't pile up because the next poll is scheduled only AFTER the request returns)". **NO migrar**: la cadena secuencial es deliberada para evitar pile-up. Sólo añadir visibility-pause via wrapper ligero. |
| Playground | `views/playground.ts:2125` | setTimeout | ❌ NO | Reset de UI específico |
| UI utils | `lib/ui-utils.ts:18` | setTimeout 1.5s | ❌ NO | Button flash helper |
| WS reconnect | `state/ws.ts:83` | setTimeout (backoff) | ❌ NO | Backoff exponencial, no periódico |
| WS heartbeat | `state/ws.ts:131` | setInterval | ❌ NO | Cleanup propio en disconnect, lógica crítica de WS |
| `model-handlers.ts:193, 195` | setTimeout 100ms | ❌ NO | Delay para `showApiError` (evita race con toast anterior) |

**Decisión**: Solo **2 timers son realmente migrables**: `vacuumPollHandle` y `pollHandle` (debug-logs, con adaptación). El resto debe quedarse como está por diseño.

| Accion | Archivos |
|---|---|
| CREAR | `lib/visibility-interval.ts` (~40 LOC) — `suppressMissedTicks(callback, intervalMs): Handle` |
| MODIFICAR | `views/config.ts` (vacuum poll) |
| MODIFICAR | `views/debug-logs.ts` (wrap `pollHandle` con visibility-pause sin cambiar la cadena secuencial) |

**Cambio de nombre (S-1)**: `visibilityAwareInterval` → `suppressMissedTicks`. El término "drift correction" es confuso; el comportamiento real es "suprimir ticks perdidos cuando la pestaña está oculta, y reanudar sin backlog".

**Criterio de aceptacion**:
- `suppressMissedTicks(callback, intervalMs)` pausa cuando `document.hidden === true` y retoma al volver a visible (ver seccion 4.3).
- Al retomar tras pausa: ejecuta el callback 1 vez inmediatamente (no N veces en backlog).
- Solo 2 timers migrados: `views/config.ts:vacuumPollHandle` y `views/debug-logs.ts:pollHandle` (con wrap ligero).
- Los demás timers permanecen sin cambios — comentario en cada uno explicando por qué.
- `pnpm typecheck` pasa.

**Impacto**: ~80 LOC no eliminados (los timers no eliminables se quedan). ~40 LOC nuevos. Ganancia principal: menor CPU en background para vacuum poll y debug-logs.

**Riesgo**: Bajo. Patrón estándar de web platform.

#### Q12: Unificar formatContext y statusPillClass

| Accion | Archivos |
|---|---|
| MODIFICAR | `components/model-table.ts` (eliminar formatContext local, importar de lib/format.ts) |
| MODIFICAR | `views/providers.ts` (eliminar formatContext local, importar de lib/format.ts) |
| VERIFICAR | `components/model-table.ts` importa statusPillClass de lib/constants.ts (ya correcto) |

**Nota verificada**: `statusPillClass` en `components/model-table.ts` ya importa correctamente de `lib/constants.ts`. Solo `formatContext` tiene 3 definiciones redundantes.

**Criterio de aceptacion**:
- `grep -rn "function formatContext" --include="*.ts"` retorna solo 1 match en `lib/format.ts`.
- `grep -rn "function statusPillClass" --include="*.ts"` retorna solo 1 match en `lib/constants.ts`.
- Las vistas providers.ts y model-table.ts importan de las fuentes canonicas.
- `pnpm typecheck` pasa.
- Los valores mostrados en UI son identicos al baseline.

**Impacto**: ~40 LOC eliminados.

**Riesgo**: Bajo. Eliminacion directa de duplicacion.

#### Q13: Crear `renderResponsiveCardTable()` (render function, NO Web Component)

| Accion | Archivos |
|---|---|
| CREAR | `components/render-responsive-card-table.ts` (~200 LOC) — `renderResponsiveCardTable(props): TemplateResult` |

**Decisión arquitectónica**: Igual que Q7, NO se usa `<responsive-card-table>` como Web Component. Se usa render function.

**Especificacion del componente**: Ver seccion 4.4.

**Criterio de aceptacion**:
- `renderResponsiveCardTable(props)` retorna un `TemplateResult`.
- El componente muestra datos en tabla en desktop y en cards en mobile.
- Recibe `columns: ColumnDef[]` y `rows: T[]`.
- Responsive automático via breakpoint de 768px (CSS via `@media`, no JS).
- 1 test unitario para render básico.
- `pnpm typecheck` pasa.

**Impacto**: ~200 LOC nuevos. Reemplazará ~500 LOC de implementaciones manuales (Q14).

**Riesgo**: Medio. El componente tiene que cubrir casos variados (acciones por fila, sort, empty state).

#### Q14: Migrar vistas a `renderResponsiveCardTable()`

| Accion | Archivos |
|---|---|
| MODIFICAR | `views/keys.ts`, `views/providers.ts`, `views/proxies.ts`, `views/proxy-sources.ts`, `views/analytics.ts` |
| VERIFICAR | Cada vista usa la render function en vez de HTML manual |

**Criterio de aceptacion**:
- Cada vista migrada usa `renderResponsiveCardTable(...)` en vez de HTML `<table>` manual.
- Layout mobile muestra cards, desktop muestra tabla.
- Ninguna vista migrada tiene `<table class="...">` con media queries manuales.
- `pnpm typecheck` pasa.
- Visual: las vistas son funcionales y responsivas.

**Impacto**: ~500 LOC eliminados, ~200 LOC de configuración nueva (neto -300).

**Riesgo**: Medio-Alto. Migrar 5 vistas visuales es laborioso. PR dedicado por vista si es necesario.

---

### FASE 3 — Scaffolding (1 PR)

#### Q15: Generalizar createView()

| Accion | Archivos |
|---|---|
| CREAR | `lib/view-factory.ts` (~80 LOC) |
| MODIFICAR | ~10 vistas que usan patron fetch+loading+empty+error |

**Especificacion**: Ver seccion 4.5.

**Criterio de aceptacion**:
- `createView<T>({loader, render, empty, error?})` genera una vista completa con estados loading, empty, error y success.
- Al menos 5 vistas migradas al nuevo patron.
- Cada vista migrada tiene el mismo comportamiento que antes.
- `pnpm typecheck` pasa.

**Impacto**: ~400 LOC eliminados (5 vistas * ~80 LOC boilerplate cada una).

**Riesgo**: Bajo-Medio. Patron trivial pero requiere verificar cada vista migrada.

#### Q17: Limpiar charts.ts

| Accion | Archivos |
|---|---|
| MODIFICAR | `components/charts.ts` (eliminar exports muertos) |
| VERIFICAR | Solo se exportan las funciones que tienen importers |

**Criterio de aceptacion**:
- `grep -rn "from.*charts" --include="*.ts"` identifica todos los imports activos.
- Solo esas funciones se exportan.
- `pnpm typecheck` pasa.

**Impacto**: ~200 LOC eliminados.

**Riesgo**: Bajo. Verificacion mecanica de imports.

#### Q18: Agregar 12 iconos de navegación a icons.ts (NO son dead — sidebar los usa)

| Accion | Archivos |
|---|---|
| MODIFICAR | `lib/icons.ts` (agregar 12 SVGs del sidebar) |
| MODIFICAR | `components/sidebar.ts` (reemplazar `navIconSvg()` inline con llamadas a `icons.xxx()`) |

**Estado verificado (2026-09-06)**:
- `lib/icons.ts` ya tiene **30 iconos** registrados (`grep -c "^\s\+[a-zA-Z]\+: \(cls" lib/icons.ts` = 30 nombres).
- `components/sidebar.ts:71-89` define una función `navIconSvg(name)` con **12 SVGs inline** que NO existen en `icons.ts`: `home`, `providers`, `combos`, `keys`, `playground`, `proxies`, `proxy-sources`, `analytics`, `logs`, `debug-logs`, `config`, `notifications`.

**Criterio de aceptacion**:
- `lib/icons.ts` añade los 12 iconos listados con nombres kebab-case consistentes (e.g. `navHome`, `navProviders`, etc.).
- `components/sidebar.ts` ya NO contiene SVGs inline — solo llamadas a `icons.xxx()` desde la nueva sección "Navigation" del objeto `icons`.
- Los iconos se ven idénticos al baseline.
- `pnpm typecheck` pasa.
- `grep -c "<svg" components/sidebar.ts` ≤ 0 (todos los `<svg>` vienen de `icons.ts`).

**Impacto**: ~150 LOC movidos de sidebar a icons.

**Riesgo**: Bajo. Movimiento mecánico + verificación visual.

---

### FASE 4 — Split Monolitos (4 PRs, 1 por sprint)

#### Q19: log-detail.ts (2004L -> 6 modulos)

| Accion | Archivos |
|---|---|
| DIVIDIR | `components/log-detail.ts` -> `components/log-detail/` directorio |
| CREAR | `components/log-detail/orchestrator.ts` (coordina sub-componentes) |
| CREAR | `components/log-detail/header.ts` (titulo, timestamp, status) |
| CREAR | `components/log-detail/messages.ts` (renderizado de mensajes) |
| CREAR | `components/log-detail/tool-calls.ts` (tool calls display) |
| CREAR | `components/log-detail/metrics.ts` (metricas y timing) |
| CREAR | `components/log-detail/actions.ts` (botones de accion) |

**Criterio de aceptacion**:
- Ningun archivo en `components/log-detail/` tiene > 500 LOC.
- `components/log-detail/orchestrator.ts` importa de los 5 sub-modulos.
- El componente sigue exponiendo la misma interfaz que antes (misma tag name, mismos props).
- `pnpm typecheck` pasa.
- Todos los flujos de log-detail funcionan igual (expand/collapse, tool calls, metrics).

**Impacto**: 2004 LOC -> 6 archivos de ~300 LOC cada uno.

**Riesgo**: Alto. Dividir un monolito de 2004L requiere entender todas las dependencias internas. Mitigar: paso 1 = extraer funciones puras, paso 2 = extraer template sections, paso 3 = separar archivos.

#### Q20: playground.ts (3381L -> 4 sub-rutas + markdown lib)

| Accion | Archivos |
|---|---|
| DIVIDIR | `views/playground.ts` -> `views/playground/` directorio |
| CREAR | `views/playground/chat.ts` (chat UI) |
| CREAR | `views/playground/image.ts` (image generation) |
| CREAR | `views/playground/embedding.ts` (embedding UI) |
| CREAR | `views/playground/audio.ts` (audio UI) |
| CREAR | `lib/markdown.ts` (markdown rendering extraido) |

**Criterio de aceptacion**:
- Ningun archivo en `views/playground/` tiene > 600 LOC.
- Cada sub-ruta funciona independientemente.
- Navegacion entre sub-rutas funciona sin recarga.
- `lib/markdown.ts` se puede importar desde otros modulos si es necesario.
- `pnpm typecheck` pasa.
- **Prerrequisito para lazy-loading (Q16)**: el split de `playground/` debe preceder a Q16. Si Q16 está deferred, playground sigue cargando eagerly desde el chunk principal hasta que se active Q16.

**Impacto**: 3381 LOC -> 5 archivos de ~600-700 LOC cada uno.

**Riesgo**: Alto. El monolito mas grande del proyecto. Mitigar: dividir por mode (chat/image/embedding/audio) primero, luego extraer markdown.

#### Q21: providers.ts (1481L -> list + detail + form)

| Accion | Archivos |
|---|---|
| DIVIDIR | `views/providers.ts` -> `views/providers/` directorio |
| CREAR | `views/providers/list.ts` (tabla de providers) |
| CREAR | `views/providers/detail.ts` (detalle de un provider) |
| CREAR | `views/providers/form.ts` (formulario de add/edit provider) |

**Criterio de aceptacion**:
- Ningun archivo tiene > 500 LOC.
- Providers list, detail y form funcionan correctamente.
- `pnpm typecheck` pasa.

**Impacto**: 1481 LOC -> 3 archivos de ~450 LOC cada uno.

**Riesgo**: Medio. providers.ts tiene logica de formulario compleja.

#### Q22: notifications.ts (1330L -> list + DnD)

| Accion | Archivos |
|---|---|
| DIVIDIR | `views/notifications.ts` -> `views/notifications/` directorio |
| CREAR | `views/notifications/list.ts` (lista de notificaciones) |
| CREAR | `views/notifications/dnd-overlay.ts` (drag and drop overlay) |

**Criterio de aceptacion**:
- Ningun archivo tiene > 500 LOC.
- DnD funciona correctamente (drag to reorder, drop zone).
- `pnpm typecheck` pasa.

**Impacto**: 1330 LOC -> 2 archivos de ~600 LOC cada uno.

**Riesgo**: Medio. DnD tiene dependencias de DOM events que pueden complicar la extracción.

#### Q16: Router lazy-loading (movido aquí desde Fase 3)

**Bloqueador identificado (ISSUE-4)**: el spec original proponía lazy-loading en Fase 3, pero `build.mjs` actual tiene limitaciones:

```js
// build.mjs (estado actual, verificado 2026-09-06)
const options = {
  entryPoints: [join(srcDir, 'app.ts')],
  bundle: true,
  format: 'esm',
  outfile: join(outDir, 'app.js'),  // SINGLE outfile, NO entryPoints múltiples
  // splitting: false (default) — NO code-splitting habilitado
};
```

**Estado real de los assets**:
- `admin_ui.rs::serve_asset()` YA sirve cualquier archivo embebido via `rust-embed` con `path` arbitrario. **Chunks ESM adicionales funcionarían sin cambios Rust** siempre que se sirvan bajo `/admin/dist/chunks/*.js`.
- `index.html` referencia `/admin/dist/app.js`. Para chunks ESM con `import.meta.url`, el bootstrap debe usar `<script type="module">` con `import("./app.js")`.

**Estrategia recomendada** (mover a Fase 4 por su interdependencia con Q20 — split de `playground.ts`):

| Paso | Acción | Archivo |
|---|---|---|
| 1 | `build.mjs`: añadir `splitting: true`, `format: 'esm'`, `outdir: 'src/static/dist'` (reemplaza `outfile`), `entryPoints: ['app.ts']` (mantener), `chunkNames: 'chunks/[name]-[hash]'` | `build.mjs` |
| 2 | `app.ts`: detectar chunks via `import.meta.url` resolution | `src/static/src/app.ts` |
| 3 | `index.html`: añadir fallback para `<noscript>` users | `src/static/index.html` |
| 4 | `admin_ui.rs`: añadir test E2E que verifique `DashboardAssets::get("dist/chunks/playground-XXX.js")` resuelve OK | `crates/openproxy-server/src/admin_ui.rs` |
| 5 | Router: registrar rutas lazy via `() => import('./views/playground.js')` | `src/static/src/lib/router.ts` (modificado) |

**Alternativa más simple** (si los cambios son demasiado riesgosos): **manual chunk splitting** vía `splitting: false` + múltiples entry points + `<script type="module">` cargados bajo demanda. Cada vista monolítica (playground, log-detail) se convierte en un entry point separado que se carga dinámicamente.

**Decisión final**: Si ninguna alternativa es viable, **DEFERIR** lazy-loading a un sprint posterior y documentar como tech-debt en `docs/tech-debt.md`.

| Accion | Archivos |
|---|---|
| MODIFICAR | `build.mjs` (activar `splitting: true` + `format: 'esm'` + `outdir` + `chunkNames`) |
| MODIFICAR | `src/static/index.html` (bootstrap ESM con fallback noscript) |
| MODIFICAR | `src/static/src/app.ts` (chunk resolution) |
| MODIFICAR | `src/static/src/lib/router.ts` (rutas lazy) |
| TEST | `crates/openproxy-server/src/admin_ui.rs` (test E2E de `DashboardAssets::get("dist/chunks/playground-XXX.js")`) |

**Prerrequisito**: Q20 (split de `playground.ts`) debe estar completo, sino el chunk contendrá todo el monolito.

**Criterio de aceptacion**:
- `build.mjs` produce `dist/app.js` + `dist/chunks/playground-[hash].js` separado.
- `DashboardAssets::get("dist/chunks/playground-XXX.js")` resuelve OK en runtime.
- Network tab en browser: `playground-XXX.js` carga solo cuando el usuario navega a `#/playground`.
- `pnpm typecheck` + `pnpm build` + `cargo test -p openproxy-server` pasan.
- Lighthouse Performance score no regresiona.

**Riesgo**: Medio-Alto. esbuild code-splitting + rust-embed requieren verificación E2E. **Si los cambios son demasiado riesgosos, DEFERIR** a sprint posterior y documentar como tech-debt.

**Impacto**: Bundle principal ~150KB más ligero.

---

### FASE 5 — Hardening (paralelo)

#### Q23: prefers-reduced-motion global

| Accion | Archivos |
|---|---|
| CREAR | `lib/motion.ts` (~20 LOC) |
| MODIFICAR | `src/static/css/views.css` (regla global) |

**Criterio de aceptacion**:
- `@media (prefers-reduced-motion: reduce)` desactiva todas las animaciones y transiciones.
- Animaciones de modales, toasts y transiciones de vista se respetan.
- Solo 1 definicion de la regla (no duplicada en multiples archivos).

**Impacto**: ~20 LOC nuevos, ~50 LOC modificados en CSS.

**Riesgo**: Bajo.

#### Q24: Extracción de strings para i18n (sistema existente, NO crear nuevo)

| Accion | Archivos |
|---|---|
| **NO CREAR** | `i18n/en.json` — **YA EXISTE** con 291 entries (`wc -l crates/openproxy-server/web/src/static/src/i18n/en.json` = 291 entries reales, vs el spec original que asumía ~200). |
| **NO CREAR** | El lookup function `t()` — **YA EXISTE** en `i18n/index.ts`. |
| MODIFICAR | `lib/constants.ts` (migrar `STAGE_LABELS` de español a `t()` calls — es el ÚNICO punto con strings hardcoded en español) |
| MODIFICAR | 8 vistas: providers, combos, keys, config, playground, key-usage, debug-logs, proxy-sources (extraer strings hardcoded adicionales y añadirlos a `en.json`) |

**Estado verificado (2026-09-06)**:
- `i18n/en.json` existe con **291 entries** (`grep -c '":' crates/openproxy-server/web/src/static/src/i18n/en.json` = 291).
- **9 archivos** importan `t()` con **193 call-sites** (`grep -rln "from.*i18n/index" src/static/src/ | wc -l` = 9, `grep -rn "\bt(\"" src/static/src --include="*.ts" | grep -v ".test.ts" | wc -l` = 193).
- Archivos que ya usan `t()`: `views/proxies.ts`, `views/login.ts`, `views/analytics.ts`, `views/notifications.ts`, `views/home.ts`, `app.ts`, `handlers/proxy-handlers.ts`, `state/notifications-store.ts`, `components/sidebar.ts`.
- `lib/constants.ts:9-18` define `STAGE_LABELS` en **español** (8 strings):
  ```ts
  export const STAGE_LABELS = {
    started: "procesando payload",
    connecting: "conectando a upstream",
    waiting_ttft: "esperando ttft",
    streaming: "recibiendo streaming",
    completed: "completado",
    failed: "falló",
    cancelled: "cancelado",
    predict_skipped: "predict skipped",
  } as const;
  ```

**Criterio de aceptacion**:
- `STAGE_LABELS` en `lib/constants.ts` se reemplaza por llamadas a `t()` (e.g. `t("stage.started")` etc.). Las 8 keys se añaden a `en.json`.
- 8 vistas adicionales (providers, combos, keys, config, playground, key-usage, debug-logs, proxy-sources) tienen sus strings hardcoded migrados a `t()` calls.
- Cada nueva key se añade primero a `i18n/en.json` antes de su uso en TS (orden estricto).
- `grep -c "STAGE_LABELS" lib/constants.ts` ≤ 0 (constante eliminada).
- `pnpm typecheck` pasa.
- UI se ve idéntica (strings en inglés).

**Impacto**: ~400 LOC modificados (strings hardcoded → lookup calls). **Sin creación** de archivos nuevos.

**Riesgo**: Medio. Hay ~200 strings por migrar; algunos viven en templates lit-html con interpolación `${variable}` que requieren cuidado para no romper la renderización.

#### Q25: Tests unitarios

| Accion | Archivos |
|---|---|
| CREAR | `tests/handlers/*.test.ts` (tests para cada handler file con `alert/confirm/prompt` migrado) |
| CREAR | `tests/lib/*.test.ts` (clipboard, mutate, visibility-interval, show-confirm, view-factory) |
| CREAR | `tests/components/*.test.ts` (render-op-modal, render-responsive-card-table) |
| EXTENDER | `tests/lib/format.test.ts` (existente) con casos edge adicionales |
| NO TOCAR | `state/*.test.ts` (5 archivos pre-existentes) y `tests/e2e/*.spec.ts` (17 specs) |

**Criterio de aceptacion**:
- `pnpm test` pasa con 0 errores (incluye los 5 archivos pre-existentes + nuevos).
- Cobertura >= 40% en: `lib/`, `handlers/`, `components/`.
- Test para: `copyToClipboard`, `mutateAndRefresh`, `suppressMissedTicks`, `showConfirm`, `showPrompt`, `renderOpModal`, `renderResponsiveCardTable`, `createView`, `formatContext`, `statusPillClass`.
- Test para cada handler con `alert()/confirm()/prompt()` migrado (verifica que no los llama).
- Los 5 unit test files pre-existentes siguen pasando sin cambios.

**Impacto**: ~1500 LOC de tests nuevos.

**Riesgo**: Bajo. Tests no afectan producción.

---

## 4. Contratos e Interfaces

### 4.1 `renderOpModal(props)` — render function

**NO es un Web Component**. Es una render function de lit-html consistente con `modal()` existente (`components/modal.ts:32`).

**Import**: `import { renderOpModal } from './components/render-op-modal.js'`

**Firma**: `renderOpModal(props: OpModalProps): TemplateResult`

**OpModalProps**:
| Campo | Tipo | Default | Descripcion |
|---|---|---|---|
| `title` | `string` | `''` | Título del modal |
| `body` | `string` (HTML) | `''` | Contenido HTML del body (wrap via `unsafeHTML` internamente) |
| `actions` | `ModalAction[]` | `[]` | Botones de acción |
| `danger` | `boolean` | `false` | Si true, botón de acción principal es rojo |
| `onClose` | `() => void` | — | Callback al cerrar (Escape, backdrop, X) |

**ModalAction**:
| Campo | Tipo | Descripcion |
|---|---|---|
| `label` | `string` | Texto del botón |
| `variant` | `'primary' \| 'secondary' \| 'danger'` | Estilo del botón |
| `value` | `string` | Valor retornado por `showOpModal` cuando se clickea (default: `label`) |

**Acceso desde handlers** (S-5): la función helper `showOpModal(props): Promise<string | null>` resuelve con el `value` de la acción clickeada, o `null` si se cerró sin elegir.

**Ejemplo de uso** (pseudocódigo):
```ts
import { renderOpModal, showOpModal } from './components/render-op-modal.js';
import { render } from 'lit-html';

const result = await showOpModal({
  title: 'Regenerate Key',
  body: '<p>This will invalidate the current key.</p>',
  danger: true,
  actions: [
    { label: 'Cancel', variant: 'secondary' },
    { label: 'Regenerate', variant: 'danger' },
  ],
});
if (result === 'Regenerate') await doRegenerate();
```

**Comportamiento**:
- Focus trap: Tab rotación dentro del modal.
- Escape: cierra el modal (resuelve Promise con `null`).
- Backdrop click: cierra el modal.
- `aria-modal="true"`, `role="dialog"`, `aria-labelledby` apunta al título.
- Animación de entrada: fade + scale (respetando `prefers-reduced-motion`).
- Al cerrar: resuelve Promise + cleanup del container DOM.

### 4.2 `mutateAndRefresh<T>`

**Import**: `import { mutateAndRefresh } from './lib/mutate.js'`

**Firma**: `mutateAndRefresh<T>(options: MutateOptions<T>): Promise<boolean>`

**MutateOptions<T>**:
| Campo | Tipo | Descripcion |
|---|---|---|
| `apiCall` | `() => Promise<T>` | La llamada API a ejecutar |
| `requestUpdate` | `() => void` | Callback para refrescar la UI |
| `successMessage` | `string` | Toast de éxito (opcional) |
| `errorMessage` | `string` | Prefijo del toast de error (opcional) |
| `onSuccess` | `(result: T) => void \| Promise<void>` | Callback post-éxito (opcional, e.g. para refresh de stores) |

**Comportamiento**:
1. Ejecuta `apiCall()`.
2. Si éxito: muestra toast con `successMessage` (si se provee), llama a `onSuccess(result)` si se provee, luego llama a `requestUpdate()`.
3. Si error: llama a `showApiError(error, errorMessage)` y retorna `false`.
4. Si éxito: retorna `true`.
5. Nunca lanza excepciones (siempre catchea).

**Cuándo NO usar** (ver Q10 Tier 3/4): multi-call `Promise.allSettled`, flujos OAuth, regenerate key (con feedback específico). Documentar con `// intentionally not using mutateAndRefresh because: ...`.

### 4.3 `suppressMissedTicks(callback, intervalMs)`

**Import**: `import { suppressMissedTicks } from './lib/visibility-interval.js'`

**Firma**: `suppressMissedTicks(callback: () => void | Promise<void>, intervalMs: number): VisibilityIntervalHandle`

> **Nombre actualizado (S-1)**: `visibilityAwareInterval` → `suppressMissedTicks`. El comportamiento es "suprimir ticks perdidos durante hidden, reanudar sin backlog". El término "drift correction" era confuso.

**VisibilityIntervalHandle**:
| Metodo | Descripcion |
|---|---|
| `stop()` | Detiene el intervalo permanentemente. |
| `pause()` | Pausa manualmente. |
| `resume()` | Reanuda manualmente. |

**Comportamiento**:
- Cuando `document.hidden === true`: pausa automática, no ejecuta callback.
- Cuando `document.visible`: reanuda, ejecuta callback inmediatamente y luego cada `intervalMs`.
- **Suprimir ticks perdidos**: si el intervalo debería haber ejecutado N veces durante la pausa, ejecuta 1 vez al reanudar (no N veces).
- Escucha `visibilitychange` event (se registra 1 vez globalmente, no por instancia).
- Al `stop()`: remueve el listener y cancela el timer.

**Aplicabilidad** (ver Q11): Solo 2 timers son migrables:
- `views/config.ts:vacuumPollHandle` (setInterval 5s).
- `views/debug-logs.ts:pollHandle` (setTimeout chained — wrap ligero sin cambiar la cadena secuencial).

### 4.4 `renderResponsiveCardTable(props)` — render function

**NO es un Web Component**. Render function de lit-html.

**Import**: `import { renderResponsiveCardTable } from './components/render-responsive-card-table.js'`

**Firma**: `renderResponsiveCardTable(props: ResponsiveTableProps): TemplateResult`

**ResponsiveTableProps**:
| Campo | Tipo | Descripcion |
|---|---|---|
| `columns` | `ColumnDef[]` | Definición de columnas |
| `rows` | `unknown[]` | Datos a mostrar |
| `emptyMessage` | `string` | Mensaje cuando rows está vacío |
| `primaryKey` | `string` | Campo del row para key único |

**ColumnDef**:
| Campo | Tipo | Descripcion |
|---|---|---|
| `key` | `string` | Campo del row |
| `label` | `string` | Header de columna |
| `render` | `(value, row) => TemplateResult` | Render custom (opcional) |
| `mobileLabel` | `string` | Label en vista card mobile (opcional, default: `label`) |
| `sortable` | `boolean` | Habilita sort por columna (opcional) |
| `hiddenMobile` | `boolean` | Oculta en vista mobile (opcional) |

**Comportamiento**:
- Desktop (>768px): renderiza `<table>`.
- Mobile (<=768px): renderiza cards con label: value por campo.
- Sorting: click en header sort (si `sortable=true`), alterna asc/desc.
- Empty state: muestra `emptyMessage` centrado.
- CSS: usa tokens CSS existentes (var(--fs-*), var(--color-*), etc.).

### 4.5 `createView<T>`

**Import**: `import { createView } from './lib/view-factory.js'`

**Firma**:
```
createView<T>(options: ViewFactoryOptions<T>): LitElement
```

> **Único uso de LitElement en todo el refactor** (justificado): la vista es stateful (ciclo loader → render → refresh interval) y se beneficia del lifecycle de LitElement. El proyecto actualmente NO usa LitElement en ningún otro sitio (`grep -rln "extends LitElement" src/` = 0), así que esto introduce una pequeña dependencia. **Alternativa evaluada y descartada**: una factory function que retorne `{ render(container): handle }` con cleanup manual, demasiado verbosa para el ahorro.

**ViewFactoryOptions<T>**:
| Campo | Tipo | Descripcion |
|---|---|---|
| `loader` | `() => Promise<T>` | Fetch de datos |
| `render` | `(data: T) => TemplateResult` | Render de datos |
| `empty` | `() => TemplateResult` | Estado vacío |
| `error` | `(err: Error) => TemplateResult` | Estado de error (opcional, default: toast + empty) |
| `refreshInterval` | `number` | Auto-refresh en ms (opcional) |
| `title` | `string` | Título de la vista (opcional) |

**Comportamiento**:
- Al montar: ejecuta `loader()`, muestra skeleton/loading, luego `render(data)` o `empty()`.
- Si `refreshInterval` se provee: usa `suppressMissedTicks` para auto-refresh.
- Manejo de error: muestra `error(err)` o fallback con toast + empty state.
- Retry: botón "Retry" en estado de error que re-ejecuta `loader()`.

### 4.6 Lazy Route Integration (Q16, Fase 4)

**Patrón** (consistente con Q16):
```ts
const routes = [
  { path: "/playground", component: () => import("./views/playground/index.js") },
  { path: "/providers",  component: () => import("./views/providers/list.js") },
  // ... resto de vistas lazy-loaded tras split monolitos (Q19-Q22)
];
```

**Integracion con router**:
- El router registra rutas con `component` como `() => Promise<{ default: RenderFunction }>` (no CustomElementConstructor — esto era incorrecto en el spec original).
- Al navegar: dynamic import, luego `render(componentResult.default(), container)`.
- Shared modules (lit-html, state) se mantienen en el chunk principal.
- Solo el view code se separa en chunks lazy.

**Restriccion**: Solo playground se lazy-load inicialmente. Otras vistas permanecen en el chunk principal hasta completar Q19-Q22 (split monolitos). Cada split monolítico introduce un nuevo chunk lazy para esa vista.

---

---

## 5. Mapeo de Breakpoints

### Declaracion actual (7 breakpoints)

```css
/* 480px  */ @media (max-width: 480px)
/* 639px  */ @media (max-width: 639px)
/* 768px  */ @media (max-width: 768px)
/* 900px  */ @media (max-width: 900px)
/* 1023px */ @media (max-width: 1023px)
/* 1080px */ @media (max-width: 1080px)
/* 1100px */ @media (max-width: 1100px)
```

### Declaracion objetivo (4 breakpoints, mobile-first)

```css
/* 640px  */ @media (min-width: 640px)
/* 768px  */ @media (min-width: 768px)
/* 1024px */ @media (min-width: 1024px)
/* 1280px */ @media (min-width: 1280px)
```

### Tabla de mapeo

| Actual | Nuevo | Afecta | Notas |
|---|---|---|---|
| 480px | 640px | Mobile phone -> small tablet | Unificar mobile extrema con tablet pequena |
| 639px | 640px | Mobile -> tablet | Caso identico, redondear |
| 768px | 768px | Tablet -> laptop | Mantener (estandar web) |
| 900px | 768px o 1024px | Depende del contexto | Evaluar caso por caso |
| 1023px | 1024px | Laptop -> desktop | Redondear a estandar |
| 1080px | 1024px o 1280px | Depende del contexto | Evaluar caso por caso |
| 1100px | 1280px | Desktop amplio | Redondear a estandar |

### Migracion por vista

| Archivo | Breakpoints actuales | Breakpoints nuevos |
|---|---|---|
| `views.css` (general) | 480, 639, 768, 900, 1023, 1080, 1100 | 640, 768, 1024, 1280 |
| `components/sidebar.ts` | Verificar | 640, 768, 1024 |
| `views/playground.ts` | Verificar | 768 |
| `views/logs.ts` | Verificar | 768, 1024 |
| `views/debug-logs.ts` | Verificar | 768 |
| `views/providers.ts` | Verificar | 768, 1024 |
| `views/keys.ts` | Verificar | 768 |
| `components/log-detail.ts` | Verificar | 768, 1024 |

**Proceso de migracion**:
1. Buscar todos los `@media` en CSS y TS.
2. Para cada uno, determinar cual de los 4 nuevos breakpoints se acerca mas.
3. Cambiar de max-width a min-width (mobile-first).
4. Verificar visualmente en 4 viewports.

---

## 6. Mapeo de Tokens CSS

> **Auditoría (2026-09-06)**: el spec original listaba tokens "a crear" que **ya existen**. La tabla siguiente refleja el estado REAL verificado en `tokens.css`. Solo se crean los tokens realmente faltantes.

### Tokens YA EXISTENTES en `tokens.css` (NO crear)

| Token | Valor real | Notas |
|---|---|---|
| `--fs-xs` | `0.72rem` | **No** `--font-size-xs` |
| `--fs-sm` | `0.85rem` | **No** `--font-size-sm` |
| `--fs-md` | `0.95rem` | **No** `--font-size-base` |
| `--fs-lg` | `1.1rem` | **No** `--font-size-lg` |
| `--fs-xl` | `1.3rem` | **No** `--font-size-xl` |
| `--fs-2xl` | `1.7rem` | **No** `--font-size-2xl` |
| `--fs-3xl` | `2.1rem` | **No** `--font-size-3xl` |
| `--radius-0` | `0` | — |
| `--radius-sm` | `4px` | **No** `0.25rem` |
| `--radius-md` | `6px` | **No** `0.375rem` |
| `--radius-lg` | `10px` | **No** `0.5rem` |
| `--radius-pill` | `9999px` | — |
| `--shadow-sm` | `0 1px 2px rgba(16, 24, 40, 0.05)` | — |
| `--shadow-md` | `0 10px 30px rgba(16, 24, 40, 0.08)` | — |
| `--color-primary` | `var(--c-dell-red)` (#e91d2a) | **NO es indigo**. Ya es Dell red. |
| `--color-primary-hover` | `#c91825` | — |
| `--color-primary-fg` | `var(--c-canvas)` | — |
| `--color-primary-soft` | `#f6dada` | — |

### Tokens a crear (SOLO los realmente faltantes)

| Token | Valor propuesto | Uso |
|---|---|---|
| `--shadow-lg` | `0 20px 40px rgba(16, 24, 40, 0.12)` | Sombras grandes (modales) |
| `--shadow-xl` | `0 30px 60px rgba(16, 24, 40, 0.16)` | Sombras extra grandes (overlays) |

### Tokens a corregir

| Token | Estado | Acción |
|---|---|---|
| ~~`--color-primary` #6366f1 (indigo, fallback)~~ | **FALSO** — ya es Dell red | Ninguna. Eliminar este claim del spec. |

### Tokens a eliminar / consolidar

| Acción | Razon |
|---|---|
| Eliminar `!important` donde se pueda (target: <500 en F1-F3, <200 en F5, desde baseline 1,719) | Usar especificidad CSS en vez de `!important` |
| Consolidar `--space-*` (no existe aún; considerar añadir `--space-1..8` si se justifica en F1-F3) | Spacing consistente |

### Migracion de font-sizes hardcoded (Q9)

- `grep -c "font-size" src/static/styles/views.css` → **346 matches** (no ~90).
- Cada match debe reemplazarse por `var(--fs-*)` apropiado:
  - `<0.85rem` → `--fs-xs`
  - `0.85-0.94rem` → `--fs-sm`
  - `0.95-1.09rem` → `--fs-md`
  - `1.10-1.29rem` → `--fs-lg`
  - `1.30-1.69rem` → `--fs-xl`
  - `1.70-2.09rem` → `--fs-2xl`
  - `>=2.10rem` → `--fs-3xl`

---

## 7. Plan de Testing

### Cobertura meta por fase

| Fase | Tests nuevos | Archivos de test | Coverage meta |
|---|---|---|---|
| Fase 1 | 2 | `tests/lib/clipboard.test.ts`, `tests/components/render-op-modal.test.ts` | 0% -> 5% |
| Fase 2 | 4 | `tests/lib/mutate.test.ts`, `tests/lib/visibility-interval.test.ts`, `tests/lib/show-confirm.test.ts`, `tests/components/render-responsive-card-table.test.ts` | 5% -> 15% |
| Fase 3 | 1 | `tests/lib/view-factory.test.ts` | 15% -> 18% |
| Fase 4 | 4 | `tests/components/log-detail.test.ts`, `tests/views/playground.test.ts`, `tests/views/providers.test.ts`, `tests/views/notifications.test.ts` (incluye test E2E de chunk-splitting de `admin_ui.rs`) | 18% -> 28% |
| Fase 5 | 8+ | `tests/handlers/*.test.ts` (cada handler file), `tests/lib/format.test.ts` (extend existente) | 30% -> 40% |

### Tests por modulo nuevo

| Modulo | Tipo de test | Qué verificar |
|---|---|---|
| `lib/clipboard.ts` | Unit | `copyToClipboard` llama `navigator.clipboard.writeText`. Fallback a `execCommand`. Error handling. |
| `lib/mutate.ts` | Unit | Éxito → toast + requestUpdate. Error → `showApiError`. No lanza excepciones. Verifica Tier 1/2/3 excluyendo Tier 4. |
| `lib/visibility-interval.ts` | Unit | `suppressMissedTicks` pausa en hidden. Retoma en visible. Suprime backlog. Stop limpio. |
| `lib/show-confirm.ts` | Unit | `showConfirm` / `showPrompt` retornan Promise. Resuelven con value de la acción clickeada. |
| `lib/view-factory.ts` | Unit | Loader exitoso → render. Loader error → error state. Empty → empty state. |
| `components/render-op-modal.ts` | Unit+DOM | `renderOpModal` retorna TemplateResult válido. Focus trap. Escape resuelve Promise(null). Backdrop cierra. Acciones retornan `value`. |
| `components/render-responsive-card-table.ts` | Unit+DOM | `renderResponsiveCardTable` → table en desktop, cards en mobile. Empty state. Sorting. |
| `lib/markdown.ts` (extraído de playground, Q20) | Unit | Renderiza headings, code blocks, links sin XSS. |

### Tests pre-existentes a NO romper (verificado 2026-09-06)

- `lib/format.test.ts` (existente)
- `state/ws.test.ts` (existente)
- `state/auth.test.ts` (existente)
- `state/notifications-store.test.ts` (existente)
- `state/live-logs-store.test.ts` (existente)
- 17 specs e2e en `tests/e2e/*.spec.ts` (Playwright)

### Comandos

```bash
pnpm --dir crates/openproxy-server/web test
pnpm --dir crates/openproxy-server/web test -- --coverage
```

---

## 8. Plan de Migracion i18n

> **Auditoría (2026-09-06)**: `i18n/en.json` YA EXISTE con 291 entries. 9 archivos YA importan `t()` con 193 call-sites. Este plan es incremental al sistema existente, no greenfield.

### Orden de extraccion (incremental, NO greenfield)

| Paso | Vistas/Modulos | Strings estimados | Estado previo |
|---|---|---|---|
| 1 | `lib/constants.ts` (STAGE_LABELS español → t()) | 8 | NO migrado |
| 2 | `views/keys.ts` | ~15 | Parcial (ya usa `t()` en algunos sitios) |
| 3 | `views/providers.ts` | ~25 | Idem |
| 4 | `views/combos.ts` | ~20 | Idem |
| 5 | `views/config.ts` | ~15 | NO migrado |
| 6 | `views/debug-logs.ts` | ~10 | NO migrado |
| 7 | `views/key-usage.ts` | ~10 | NO migrado |
| 8 | `views/proxy-sources.ts` | ~10 | NO migrado |
| 9 | `views/playground.ts` | ~30 | NO migrado (monolito, último) |
| 10 | `handlers/*.ts` (error messages en `alert()` restantes) | ~25 | Parcial |
| 11 | `components/*.ts` (labels, tooltips) | ~20 | Parcial |

**Total estimado**: ~188 strings nuevos en `en.json` (de 291 → ~479).

### Patron de migracion

1. Identificar string hardcoded en template lit-html: `"Some text"`.
2. Agregar a `i18n/en.json`: `"some.text": "Some text"`.
3. Reemplazar en template: `t('some.text')`.
4. Verificar que la interpolación `${...}` no se rompe.
5. Para `STAGE_LABELS` (español): el patrón es ligeramente diferente — la constante exportada se reemplaza por un map que llama a `t()`:
   ```ts
   // Antes:
   export const STAGE_LABELS = { started: "procesando payload", ... };
   // Después:
   export function stageLabel(stage: StageKey): string {
     return t(`stage.${stage}`);
   }
   ```
   Y se eliminan los call-sites `STAGE_LABELS[stage]` reemplazándolos por `stageLabel(stage)`.

### Convenciones de keys (existentes)

- `domain.entity.property`: `keys.form.name`, `providers.list.title`, `stage.started`
- Lowercase, dot-separated.
- Prefijo del dominio primero.

### Verificación baseline (2026-09-06)

```bash
# Estado actual
grep -c '":' crates/openproxy-server/web/src/static/src/i18n/en.json   # → 291
grep -rln "from.*['\"].*i18n/index" src/static/src/ | wc -l              # → 9
grep -rn "\bt(\"" src/static/src/ --include="*.ts" | grep -v ".test.ts" | wc -l  # → 193
```

---

---

## 9. Plan de Rollback

### Estrategia general

Cada fase es un PR independiente. Si una fase rompe algo, revertir el PR completo.

### Comando de revert

```bash
git revert <commit-hash>
git push
```

### Rollback por fase

| Fase | Riesgo de regression | Rollback tiempo | Notas |
|---|---|---|---|
| Fase 1 | Bajo | < 5 min | Solo eliminacion de dead code y centralizacion. Si falla, revert del PR. |
| Fase 2 | Medio | < 5 min | Cambios en manejo de datos. Revert rapido. |
| Fase 3 | Bajo | < 5 min | Scaffolding (createView) y tests. Sin code-splitting. |
| Fase 4 | Alto | < 10 min | División de monolitos + Q16 lazy-loading. Cada monolito y Q16 son PRs separados; revert individual. Si Q16 falla, playground sigue en chunk principal. |
| Fase 5 | Bajo | < 5 min | Hardening. Si tests fallan, revert. Si CSS breaks, revert. |

### Checkpoints de validacion post-revert

1. `pnpm --dir crates/openproxy-server/web typecheck` pasa.
2. `pnpm --dir crates/openproxy-server/web build` completa.
3. Dashboard carga en browser.
4. Todas las vistas principales son funcionales.

---

## 10. Definition of Done Global

### Verificaciones automaticas

| Verificacion | Comando | Resultado esperado |
|---|---|---|
| Typecheck | `pnpm --dir crates/openproxy-server/web typecheck` | Sin errores |
| Build | `pnpm --dir crates/openproxy-server/web build` | < 500KB bundle |
| Clippy | `cargo clippy --workspace --all-targets -- -D warnings` | Sin warnings |
| Tests | `pnpm --dir crates/openproxy-server/web test` | Todos pasan |

### Verificaciones manuales

| Verificacion | Como verificar |
|---|---|
| Lighthouse Performance >= 90 | Lighthouse en mobile (Moto G Power, 4G) |
| Lighthouse A11y >= 90 | Lighthouse en la misma run |
| 0 archivos > 800 LOC | `wc -l views/*.ts components/**/*.ts handlers/*.ts lib/*.ts \| sort -n \| tail` |
| 0 `alert()/confirm()/prompt()` nativos | `grep -rEn "\b(alert\|confirm\|prompt)\(" src/static/src/{views,handlers,components} \| grep -v "\.test\."` → 0 |
| 0 componentes dead-code | `for f in components/*.ts; do n=$(grep -rln "from.*$(basename $f .ts)\b" src/ \| wc -l); [ "$n" -eq 0 ] && echo "$f"; done` → vacío |
| 4 breakpoints solamente | `grep -rh "@media" src/static/styles/views.css \| grep -oE "[0-9]+px" \| sort -u` → solo `640`, `768`, `1024`, `1280` |
| < 500 `!important` en F1-F3, < 200 en F5 | `grep -c "!important" src/static/styles/views.css` |
| 5 archivos de test pre-existentes sin cambios | `pnpm test` incluye `state/ws.test.ts`, `state/auth.test.ts`, etc. y pasan |

### Checklist por PR

- [ ] `pnpm typecheck` pasa
- [ ] `pnpm build` pasa
- [ ] `cargo clippy` pasa
- [ ] `pnpm test` pasa
- [ ] Visual verification en 4 viewports (375, 768, 1024, 1440)
- [ ] No hay regresiones en funcionalidad existente
- [ ] Commit message sigue Conventional Commits

---

**Total de archivos a crear**: ~10 (Q1 no crea; Q4, Q5, Q7, Q10, Q11, Q13, Q15, Q16, Q18, Q24 no crean si la base ya existe)
**Total de archivos a eliminar**: ~11 (componentes dead-code) + 2 (`playwright-state.json`, `check_console.mjs`) = 13
**Total de archivos a modificar**: ~45 (handlers, vistas, build.mjs, tokens.css, etc.)
**LOC neto estimado**: ~22K TS + ~6K CSS (reducción de ~28% TS, ~36% CSS desde baseline verificado)

> **Nota**: la estimación de reducción de TS (~14K) del spec original era demasiado optimista. Con baseline real de 30,583 LOC y asumiendo que los splits monolíticos (Q19-Q22) solo reorganizan (no eliminan) ~8,200 LOC, y los refactors Q1+Q4+Q5+Q10+Q12+Q14+Q15+Q17+Q18+Q23 eliminan ~3,500-4,000 LOC, el target realista es ~22K LOC.
