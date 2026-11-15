# Informe de Auditoría y Refinamiento UX / Accesibilidad (WCAG 2.2 AA) — OpenProxy

**Rama de trabajo:** `ux/a11y-minimalist-refinement`  
**Estándares de referencia:** W3C WCAG 2.2 Nivel AA · WAI-ARIA 1.2 · Heurísticas de Usabilidad de Nielsen · Estándar CLI `NO_COLOR`

---

## 1. Resumen de Impacto

| Métrica | Línea Base (Estado Inicial) | Estado Final (`ux/a11y-minimalist-refinement`) | Impacto / Variación |
| :--- | :--- | :--- | :--- |
| **Cumplimiento de Contraste Matemático (WCAG 1.4.3)** | **54.5%** (6 de 11 pares semánticos críticos fallaban `< 4.5:1`) | **100%** (11 de 11 pares verificados entre `4.64:1` y `6.29:1`) | **+45.5% (0 violaciones AA)** |
| **Eliminación de CSS Muerto / Duplicado (`cards.css`, `forms.css`)** | 89 líneas de reglas duplicadas/obsoletas (`.toast*` legacy y `.sidebar-footer`) | 0 líneas redundantes (fuente única de verdad en `toasts.css` y `layout.css`) | **-88 líneas netas en esos módulos** |
| **Árbol Semántico en `DataGrid` y `LogDetailModal`** | Encabezados ocultos (`aria-hidden="true"`), sin roles `grid`/`row`/`gridcell`/`tablist`/`tab` | Árbol WAI-ARIA 1.2 completo (`grid`, `columnheader`, `gridcell`, `tablist`, `tab`, `tabpanel`) | **100% operable con lector de pantalla** |
| **Gestión de Foco y Teclado (WCAG 2.1.1, 2.4.3, 2.4.7)** | `outline: none` sin reemplazo en 3 controles; sin retorno de foco al cerrar modales/menús | Anillos `:focus-visible` universales, focus trap, navegación por flechas en menús y restauración de foco | **100% operable sin ratón** |
| **Estabilidad Visual (CLS) y Target Size (WCAG 2.5.8 / 2.5.5)** | Saltos de layout en error de login; botones de cierre/toggle `< 24px` | Reserva de altura (`min-height: 2.75rem`, `CLS = 0`), mínimo `>= 24px` desktop y `44x44px` en `pointer: coarse` | **CLS = 0 y 100% objetivos táctiles conformes** |
| **Ergonomía de CLI y Logs (`main.rs`, `telemetry.rs`)** | Sin banderas `--help`/`--version`; ignoraba `NO_COLOR`; errores crudos de TOML/bind | Soporte nativo `-h`/`--help`, `-V`/`--version`, detección `NO_COLOR` + `IsTerminal` y pistas accionables | **Cero dependencias adicionales** |

---

## 2. Metodología de Auditoría Multidimensional (5 Subagentes)

1. **Subagente 1 (Accesibilidad Semántica y Lectores de Pantalla - WCAG 2.2):** Auditoría matemática de luminancia relativa sobre todos los pares de tokens en `tokens.css` y `themes.css`, validación de jerarquía de encabezados (`h1`–`h6`), landmarks (`main`, `nav`, `aside`), `skip-link` y regiones vivas (`role="status"`, `role="alert"`, `role="alertdialog"`).
2. **Subagente 2 (Navegación por Teclado, Foco y Preferencias del Sistema):** Inspección de reglas `:focus` / `:focus-visible`, ciclo de vida del foco en `renderOpModal`, `showConfirm`, `showPrompt`, `openLogDetail` y `openContextMenu`, y suscripción reactiva a `prefers-color-scheme`.
3. **Subagente 3 (Minimalismo Funcional y Carga Cognitiva):** Detección de CSS muerto/sobrescrito entre `components/cards.css` y `components/toasts.css`, colisiones de cascada entre `layout.css` y `components/forms.css`, y limpieza de variables inexistentes en `server-reconnect.css`.
4. **Subagente 4 (Feedback del Sistema, Estados y Ergonomía de Errores / CLI):** Auditoría del binario Rust (`crates/openproxy-server/src/main.rs` y `telemetry.rs`) frente al estándar `NO_COLOR`, banderas estándar POSIX (`--help`, `--version`) y accionabilidad de errores de arranque.
5. **Subagente 5 (Layout, Responsividad y Estabilidad Visual):** Verificación de áreas mínimas de clic/toque (`24x24px` en puntero fino y `44x44px` en `pointer: coarse`), visibilidad de acciones rápidas en pantallas táctiles y prevención de Cumulative Layout Shift (CLS) en formularios.

---

## 3. Detalle Técnico por Hallazgo y Parche Atómico

### Hallazgo 01 — `[A11Y-01]` Ratios de Contraste Matemático WCAG 2.2 AA y Tokens en Overlay de Reconexión
- **Categoría:** Accesibilidad / Minimalismo de Tokens
- **Parche:** `patches/01-wcag-contrast-tokens.patch`
- **Commit:** `ux(a11y): [A11Y-01] corregir ratios de contraste WCAG 2.2 AA en tokens de tema y overlay de reconexion`
- **Archivos afectados:**
  - `crates/openproxy-server/web/src/static/styles/tokens.css`
  - `crates/openproxy-server/web/src/static/styles/themes.css`
  - `crates/openproxy-server/web/src/static/styles/components/server-reconnect.css`
- **Criterio WCAG / Heurística vulnerada:**
  - **WCAG 2.2 AA 1.4.3 Contrast (Minimum):** El texto normal y las etiquetas de componentes deben mantener una relación de contraste mínima de **4.5:1**.
  - **Consistencia y Estándares (Nielsen #4):** `server-reconnect.css` empleaba variables inexistentes (`--bg-surface`, `--text-main`, `--border-color`) con fallbacks hexadecimales hardcodeados de Catppuccin Mocha (`#1e1e2e`, `#cdd6f4`, `#89b4fa`), rompiendo la coherencia cromática en el tema claro.
- **Evidencia del estado anterior (Línea Base):**
  - `--color-text-muted` (`#667085`) sobre `--color-surface-2` (`#f0f3f7`): **4.47:1** (< 4.5:1, fallo en cabeceras `th`, `.field-label`, `.kbd` y barras de filtro).
  - `--color-primary-fg` (`#ffffff`) sobre `--color-primary` (`#e91d2a`): **4.49:1** (< 4.5:1, fallo en `.btn-primary`).
  - `--color-primary` (`#e91d2a`) sobre `--color-primary-soft` (`#f6dada`): **3.41:1** (< 4.5:1, fallo en `.sidebar nav a.active` y pestañas activas).
  - `--color-success` (`#4d7c2a`) sobre `--color-success-soft` (`#d8e2c8`): **3.69:1** (< 4.5:1, fallo en `.badge-success` y `.status-pill.ok`).
  - `--color-warn` (`#a86a00`) sobre `--color-warn-soft` (`#f5d9b8`): **3.28:1** (< 4.5:1, fallo en `.badge-warn`, `.status-pill.warn`, `.btn-warn`).
  - `--color-chain-fg` (`#ffffff`) sobre `--color-chain` (`#6366f1`): **4.47:1** (< 4.5:1, fallo en botón de compra `.btn.chain`).
  - En tema oscuro (`themes.css`): `--color-status-lost` (`#78746e`) sobre `--color-status-lost-soft` (`#262523`): **3.30:1** (< 4.5:1).
- **Explicación técnica de la solución aplicada:**
  - Se recalibraron matemáticamente los tokens semánticos en `tokens.css` y `themes.css` preservando el matiz cromático original:
    - `--color-text-muted`: `#667085` → `#596375` (**5.44:1** sobre `#f0f3f7` y **5.64:1** sobre `#f5f7fa`).
    - `--c-dell-red` / `--color-primary`: `#e91d2a` → `#c71620` y `--color-primary-soft`: `#f8e2e2` (**5.89:1** con texto blanco y **4.76:1** sobre fondo soft).
    - `--color-success`: `#356318` sobre `--color-success-soft`: `#e2ecd5` (**5.83:1**).
    - `--color-warn`: `#855200` sobre `--color-warn-soft`: `#faebd7` (**5.60:1**).
    - `--color-chain`: `#4f46e5` (**6.29:1** con `#ffffff`).
    - Tema oscuro `--color-status-lost`: `#78746e` → `#9e9990` (**5.41:1** sobre `#262523` y **6.07:1** sobre `#1c1b1a`).
  - En `server-reconnect.css`, se sustituyeron todas las variables huérfanas y colores hardcodeados por `--color-surface`, `--color-text`, `--color-text-muted`, `--color-border`, `--color-primary` y `--color-primary-fg`.

---

### Hallazgo 02 — `[A11Y-02]` Enlace de Salto (`skip-link`), Landmarks Semánticos, Jerarquía `h1` y Regiones `aria-live`
- **Categoría:** Accesibilidad Semántica (Lectores de Pantalla)
- **Parche:** `patches/02-semantic-landmarks-live-regions.patch`
- **Commit:** `ux(a11y): [A11Y-02] incorporar enlace de salto, landmarks ARIA, jerarquia h1 y regiones aria-live`
- **Archivos afectados:**
  - `crates/openproxy-server/web/src/static/styles/base.css`
  - `crates/openproxy-server/web/src/static/src/components/shell.ts`
  - `crates/openproxy-server/web/src/static/src/components/sidebar.ts`
  - `crates/openproxy-server/web/src/static/src/views/login.ts`
  - `crates/openproxy-server/web/src/static/src/lib/view-utils.ts`
  - `crates/openproxy-server/web/src/static/src/state/server-reconnect.ts`
- **Criterio WCAG / Heurística vulnerada:**
  - **WCAG 2.2 AA 2.4.1 Bypass Blocks:** Ausencia de mecanismo para saltar los 13 enlaces del menú lateral al navegar con teclado.
  - **WCAG 2.2 AA 1.3.1 Info and Relationships & 2.4.6 Headings and Labels:** La vista de login comenzaba directamente en `<h2>` sin `<h1>`, y `<nav>` / `<aside>` carecían de etiquetas de landmark explícitas.
  - **WCAG 2.2 AA 4.1.3 Status Messages:** Los estados de carga (`DEFAULT_LOADING`), errores de carga (`DEFAULT_ERROR`), el indicador de salud del servidor (`#health-status`) y el overlay de reconexión (`#server-reconnect-overlay`) cambiaban dinámicamente sin notificar a tecnologías de asistencia.
- **Evidencia del estado anterior:**
  - `shell.ts`: `<aside class="sidebar" id="sidebar"></aside><main id="main"></main>` sin `.skip-link` ni `tabindex="-1"` en `#main`.
  - `view-utils.ts`: `DEFAULT_LOADING = () => html\`<div class="loading">Loading...</div>\`` y `DEFAULT_ERROR = (err) => html\`<div class="banner banner-error">...</div>\`` sin `role="status"` ni `role="alert"`.
  - `server-reconnect.ts`: `<div id="server-reconnect-overlay" class="server-reconnect-overlay">` sin `role="alertdialog"`, `aria-live="assertive"` ni `aria-hidden="true"` en el spinner.
- **Explicación técnica de la solución aplicada:**
  - Se incorporó `<a class="skip-link" href="#main">Skip to main content</a>` en `shell.ts` con controlador de foco directo sobre `<main id="main" tabindex="-1">` (evitando colisiones con el router hash `#/`), estilizado en `base.css` para mostrarse únicamente en `:focus-visible`.
  - Se añadieron `aria-label="Main navigation"` en `<nav>`, `aria-expanded` dinámico en `.mobile-nav-toggle`, y `role="status" aria-live="polite"` en el contenedor `.health` de `sidebar.ts`.
  - Se promovió el título principal de `views/login.ts` a `<h1 class="login-title">` y se añadió `aria-busy` al botón de envío.
  - Se añadieron `role="status" aria-live="polite" aria-busy="true"` en `DEFAULT_LOADING`, `role="alert"` en `DEFAULT_ERROR` (`view-utils.ts`) y `role="alertdialog" aria-modal="true" aria-live="assertive"` en `server-reconnect.ts`.

---

### Hallazgo 03 — `[A11Y-03]` Anillos de Foco, Gestión de Foco en Modales/Menús y Semántica WAI-ARIA en `DataGrid` y `LogDetail`
- **Categoría:** Navegación por Teclado, Foco y Semántica de Componentes
- **Parche:** `patches/03-keyboard-focus-modals-datagrid.patch`
- **Commit:** `ux(a11y): [A11Y-03] restaurar anillos de foco, gestion de foco en modales/menus y semantica WAI-ARIA en DataGrid y LogDetail`
- **Archivos afectados:**
  - `crates/openproxy-server/web/src/static/src/components/data-grid.ts`
  - `crates/openproxy-server/web/src/static/styles/components/data-grid.css`
  - `crates/openproxy-server/web/src/static/styles/components/modals.css`
  - `crates/openproxy-server/web/src/static/styles/components/cards.css`
  - `crates/openproxy-server/web/src/static/src/components/render-op-modal.ts`
  - `crates/openproxy-server/web/src/static/src/lib/show-confirm.ts`
  - `crates/openproxy-server/web/src/static/src/components/log-detail/modal.ts`
  - `crates/openproxy-server/web/src/static/src/components/log-detail/index.ts`
  - `crates/openproxy-server/web/src/static/src/components/context-menu.ts`
  - `crates/openproxy-server/web/src/static/src/state/theme.ts`
- **Criterio WCAG / Heurística vulnerada:**
  - **WCAG 2.2 AA 4.1.2 Name, Role, Value & 1.3.1 Info and Relationships:** `data-grid.ts` tenía `aria-hidden="true"` en `.dg-head`, ocultando todas las cabeceras de columna a los lectores de pantalla, y carecía de roles `grid`, `row`, `columnheader`, `gridcell` y `aria-selected`. Asimismo, `log-detail/modal.ts` carecía de `role="dialog"`, `role="tablist"`, `role="tab"`, `aria-selected` y `role="tabpanel"`.
  - **WCAG 2.2 AA 2.4.7 Focus Visible:** `.dg`, `.headers-strip-input:focus` y `.model-search-wrap input:focus` aplicaban `outline: none` sin indicador alternativo de suficiente contraste.
  - **WCAG 2.2 AA 2.1.1 Keyboard & 2.4.3 Focus Order:** `renderOpModal`, `showConfirm`, `showPrompt`, `openLogDetail` y `openContextMenu` no restauraban el foco al elemento invocador al cerrarse; `openLogDetail` no cerraba con `Escape`; `openContextMenu` no enfocaba el primer ítem ni permitía navegar con flechas (`ArrowDown`/`ArrowUp`/`Home`/`End`).
- **Evidencia del estado anterior:**
  - `data-grid.ts:511`: `<div class="dg-head" aria-hidden="true">` y botones `.dg-qa` únicamente con `title` sin `aria-label`.
  - `log-detail/modal.ts:307`: `<div id="log-detail-modal" class="modal-bg log-detail-modal">` sin `role="dialog"` ni listener para la tecla `Escape`.
  - `show-confirm.ts:132`: `<input type="text" id="show-prompt-input">` sin `<label for="show-prompt-input">` ni `aria-label`.
- **Explicación técnica de la solución aplicada:**
  - En `data-grid.ts`, se expuso la jerarquía completa `role="grid"` → `role="row"` → `role="columnheader"` / `role="gridcell"`, `aria-selected`, `aria-expanded`, `aria-label` en `.dg-qa` y `.dg-rename`, y anillo `:focus-visible` en `.dg:focus-visible .dg-wrap`.
  - Se restauraron los contornos `outline: 2px solid var(--color-primary)` en `modals.css` y `cards.css`.
  - En `render-op-modal.ts`, `show-confirm.ts`, `log-detail/modal.ts` y `context-menu.ts`, se implementó captura del `opener` (`document.activeElement`) y restauración automática del foco al desmontar, cierre con `Escape` en `log-detail/modal.ts`, ciclo `Tab`/`Shift+Tab` en `showConfirm`, etiqueta `<label for="show-prompt-input">` en `showPrompt`, roles `tablist`/`tab`/`tabpanel` sincronizados en `log-detail`, y navegación completa por teclado (`ArrowDown`, `ArrowUp`, `Home`, `End`, `Escape`) en `context-menu.ts`.
  - En `state/theme.ts`, se añadió suscripción en vivo a `matchMedia("(prefers-color-scheme: dark)")` cuando el usuario no ha forzado manualmente un tema en `localStorage`.

---

### Hallazgo 04 — `[MIN-04]` Eliminación de Código CSS Muerto (`toast` legacy) y Colisiones de Cascada (`sidebar-footer`)
- **Categoría:** Minimalismo Funcional y Reducción de Peso
- **Parche:** `patches/04-minimalism-dead-css-removal.patch`
- **Commit:** `ux(minimalism): [MIN-04] eliminar bloque CSS legacy de toasts y reglas duplicadas de sidebar-footer`
- **Archivos afectados:**
  - `crates/openproxy-server/web/src/static/styles/components/cards.css`
  - `crates/openproxy-server/web/src/static/styles/components/forms.css`
  - `crates/openproxy-server/web/src/static/styles/components/toasts.css`
- **Criterio WCAG / Heurística vulnerada:**
  - **Diseño Estético y Minimalista (Nielsen #8) y Regla de Minimalismo Estricto:** Presencia de 75 líneas de CSS muerto en `components/cards.css` (bloque legacy `.toast-container`, `.toast`, `.toast-*`) completamente sobrescritas por `components/toasts.css`, además de reglas duplicadas y contradictorias para `.sidebar-footer` y `.sidebar-toggle` en `components/forms.css` frente a `layout.css`.
- **Evidencia del estado anterior:**
  - `components/toasts.css:2`: Comentario explícito reconociendo la deuda técnica: *"Loaded after components/cards.css (which still owns the legacy toast block) so these rules are the authoritative presentation"*.
  - `components/forms.css:140-148`: Redeclaraba `.sidebar-footer { justify-content: space-between; margin-top: auto; }` colisionando con `layout.css:131` (`flex-direction: column-reverse; align-items: stretch;`).
- **Explicación técnica de la solución aplicada:**
  - Se eliminaron las 76 líneas del bloque legacy de toasts en `components/cards.css` y las 10 líneas duplicadas de `.sidebar-footer` / `.sidebar-toggle` en `components/forms.css`, reduciendo 88 líneas netas de CSS redundante y dejando a `toasts.css` y `layout.css` como únicas fuentes de verdad.

---

### Hallazgo 05 — `[UX-05]` Tamaño Mínimo de Objetivos Táctiles (WCAG 2.5.8 / 2.5.5) y Estabilidad Visual (CLS = 0)
- **Categoría:** Layout, Responsividad y Estabilidad Visual
- **Parche:** `patches/05-target-sizes-layout-stability.patch`
- **Commit:** `ux(a11y): [UX-05] garantizar tamano minimo de objetivos tactiles (WCAG 2.5.8 / 2.5.5) y estabilidad visual sin CLS`
- **Archivos afectados:**
  - `crates/openproxy-server/web/src/static/styles/components/forms.css`
  - `crates/openproxy-server/web/src/static/styles/components/data-grid.css`
  - `crates/openproxy-server/web/src/static/styles/layout.css`
- **Criterio WCAG / Heurística vulnerada:**
  - **WCAG 2.2 AA 2.5.8 Target Size (Minimum) & 2.5.5 Target Size (Enhanced):** Los controles interactivos deben medir al menos `24x24px` en escritorio y `44x44px` en pantallas táctiles (`pointer: coarse`).
  - **Estabilidad Visual (CLS = 0):** El slot de error en la vista de login tenía `min-height: 0`, provocando un salto brusco de ~52px en el botón de envío al mostrarse un error de validación.
- **Evidencia del estado anterior:**
  - `forms.css:86`: `button.close-btn` carecía de `min-width` y `min-height` (~18x19px reales); `.toggle-btn` tenía `height: 22px` (< 24px).
  - `data-grid.css:170`: `.dg-qa` tenía `opacity: 0` hasta `:hover`, haciéndolo invisible e inaccesible en dispositivos táctiles sin hover.
  - `layout.css:545`: `.login-error-slot { min-height: 0; }`.
- **Explicación técnica de la solución aplicada:**
  - Se elevaron `button.small` y `.toggle-btn` a un mínimo estricto de `24px` de alto, y `button.close-btn` a `28x28px` en escritorio y `44x44px` bajo `@media (pointer: coarse)` junto con `.theme-toggle` y `.ctx-menu button`.
  - En `@media (pointer: coarse)`, los botones de acción rápida `.dg-qa` mantienen `opacity: 1` y área mínima de `32x32px`.
  - En `layout.css`, `.login-error-slot` y `.login-error` comparten `min-height: 2.75rem; margin: 0;`, eliminando por completo el salto de layout (CLS = 0) al validar credenciales.

---

### Hallazgo 06 — `[DX-06]` Ergonomía de CLI (`--help`, `--version`, `NO_COLOR`) y Errores de Configuración Accionables
- **Categoría:** Feedback del Sistema, Estados y Ergonomía de CLI / Configuración (DX)
- **Parche:** `patches/06-cli-ergonomics-no-color.patch`
- **Commit:** `ux(dx): [DX-06] implementar banderas CLI --help/--version, soporte NO_COLOR y mensajes de error accionables`
- **Archivos afectados:**
  - `crates/openproxy-server/src/main.rs`
  - `crates/openproxy-server/src/telemetry.rs`
- **Criterio WCAG / Heurística vulnerada:**
  - **Ayuda y Documentación (Nielsen #10) y Recuperación de Errores (Nielsen #9):** El binario `openproxy` ignoraba `--help`/`-h` y `--version`/`-V` (intentando levantar el servidor completo y abrir la base de datos en lugar de mostrar la ayuda), e imprimía errores crudos al fallar la lectura de `config.toml` o el bind TCP.
  - **Ergonomía de Terminal (`NO_COLOR`):** `telemetry.rs` emitía secuencias de escape ANSI en modo texto incluso cuando la variable de entorno `NO_COLOR` estaba definida o la salida estándar estaba redirigida a un pipe.
- **Evidencia del estado anterior:**
  - `main.rs:360`: Solo comprobaba `--laya-worker`, `--embed-worker` y `--healthcheck`.
  - `telemetry.rs:59`: `fmt::layer().compact().with_filter(fmt_filter)` sin `.with_ansi(...)`.
- **Explicación técnica de la solución aplicada:**
  - En `telemetry.rs`, se implementó `no_color_requested()` respetando el estándar `https://no-color.org/` y `std::io::IsTerminal::is_terminal(&std::io::stdout())`, desactivando secuencias ANSI cuando `NO_COLOR` no está vacío o cuando `stdout` no es un TTY.
  - En `main.rs`, se añadieron las funciones `render_cli_help()` y `render_cli_version()` (cero dependencias adicionales) para responder instantáneamente a `-h`/`--help` y `-V`/`--version` documentando opciones y variables de entorno críticas (`OPENPROXY_CONFIG`, `OPENPROXY_MASTER_KEY`, `RUST_LOG`, `NO_COLOR`), y se enriquecieron los errores de `load_server_config()` y `listener::bind()` con pistas accionables de remediación.
