# openproxy — Post-MVP Roadmap & Backlog

Este documento define la hoja de ruta técnica y la lista estructurada de tareas (To-Do List) para las capacidades y características planificadas posteriores al MVP de **openproxy**.

---

## 1. Matriz de Priorización de Capacidades

| Nivel | Área / Característica | Estado | Complejidad | Impacto |
| :--- | :--- | :--- | :--- | :--- |
| **P1** | **Compresión HTTP de Transporte (`gzip` / `br` / `zstd`)** | 📋 Pendiente | Baja | Alto |
| **P2** | **MCP (Model Context Protocol) & Gateway de Herramientas** | 📋 Pendiente | Media | Alto |
| **P3** | **Escalabilidad Horizontal & Estado Distribuido** | 📋 Pendiente | Alta | Alto |
| **P4** | **Memoria Persistente & Conversation Stores** | 📋 Pendiente | Media | Medio |
| **P5** | **Assistants API & Endpoints Stateful** | 📋 Pendiente | Alta | Medio |
| **P6** | **Guardrails, Evals & Content Filtering** | 📋 Pendiente | Media | Medio |
| **P7** | **Desktop App, System Tray & Distribución Nativa** | 📋 Pendiente | Alta | Bajo |

---

## 2. To-Do List Detallada por Capacidad

### 🚀 P1: Compresión HTTP de Transporte (Assets & JSON)

*Objetivo:* Reducir significativamente el ancho de banda y los tiempos de transferencia en la carga de assets embebidos del dashboard SPA y en respuestas JSON voluminosas (catálogo de modelos, logs históricos, analíticas), sin afectar la latencia TTFT de los streams SSE.

- [ ] **Middleware de Compresión en `openproxy-server`**
  - [ ] Integrar `tower-http::compression::CompressionLayer` con soporte para `gzip`, `brotli` (`br`) y `zstd`.
  - [ ] Configurar umbral mínimo de tamaño (ej. solo comprimir payloads $\ge 1\,\text{KB}$).
- [ ] **Compresión de Assets Estáticos Embebidos**
  - [ ] Habilitar compresión al vuelo o pre-compresión en tiempo de build (`.gz` / `.br`) para los bundles del frontend (`/admin/dist/*`, CSS, JS, favicons).
  - [ ] Configurar headers de caché (`Cache-Control: public, max-age=31536000, immutable` para assets hasheados).
- [ ] **Compresión de Endpoints de Datos JSON**
  - [ ] Comprimir respuestas de `GET /v1/models` (catálogos grandes de cientos de modelos).
  - [ ] Comprimir respuestas de `/admin/api/usage/*`, `/admin/api/logs` y `/admin/api/models`.
- [ ] **Bypass para Streams SSE (`text/event-stream`)**
  - [ ] Asegurar que el stream de SSE (`/v1/chat/completions` con `stream: true`) no sufra buffering de compresión para preservar la métrica de TTFT y baja latencia chunk a chunk.

---

### 🧩 P2: Soporte para MCP (Model Context Protocol) & Agentes

*Objetivo:* Conectar openproxy con el ecosistema de herramientas e interoperabilidad de agentes basados en Model Context Protocol (MCP).

- [ ] **Cliente MCP Integrado**
  - [ ] Conector para servidores MCP locales (stdio) y remotos (SSE/HTTP).
  - [ ] Descubrimiento dinámico de herramientas y recursos provistos por servidores MCP configurados.
- [ ] **Inyección y Enrutamiento de Herramientas**
  - [ ] Mapeo automático de herramientas MCP hacia el esquema `tools` de OpenAI / Anthropic / Gemini.
  - [ ] Intercepción y ejecución de `tool_calls` dirigidas a servidores MCP registrados antes de devolver la respuesta al cliente.
- [ ] **Administración en Dashboard**
  - [ ] Vista en el dashboard web para registrar y monitorear servidores MCP activos y su inventario de herramientas.

---

### 🌐 P3: Escalabilidad Horizontal & Estado Compartido

*Objetivo:* Permitir el despliegue de múltiples réplicas del binario `openproxy` detrás de un balanceador de carga L7.

- [ ] **Backend de Estado Distribuido (Opcional / Conectable)**
  - [ ] Abstracción de estado con backend pluggable: `Memory` (monoproceso local) vs `Redis` / `Key-Value Store`.
  - [ ] Sincronización de contadores atómicos de `round_robin` entre instancias.
- [ ] **Circuit Breaker y Cooldowns Distribuidos**
  - [ ] Publicación y suscripción (Pub/Sub) de eventos de degradación de cuentas y estados de salud.
  - [ ] Sincronización de ventanas de rate-limiting upstream (429 `Retry-After`).
- [ ] **Rate Limiting Distribuido de Clientes**
  - [ ] Algoritmo de token bucket / leaky bucket respaldado por Redis para cuotas por API key en cluster.

---

### 🧠 P4: Memoria Persistente, Context Stores & Vector Cache

*Objetivo:* Gestionar contexto conversacional y almacenamiento semántico en el proxy.

- [ ] **Almacenamiento de Conversaciones (`session_id` / `thread_id`)**
  - [ ] Almacenamiento persistente de historiales de chat en SQLite / base de datos externa.
  - [ ] Ventana deslizante automática de contexto y truncamiento inteligente de mensajes antiguos.
- [ ] **Semantic Caching (Caché Semántica de Prompts)**
  - [ ] Almacenamiento y búsqueda de embeddings para respuestas cacheadas frente a preguntas idénticas o semánticamente similares.
  - [ ] Invalidador de caché configurable por TTL y umbral de similitud coseno.

---

### 🤖 P5: Soporte Stateful — Assistants API & Threads

*Objetivo:* Ampliar la compatibilidad con el estándar OpenAI Assistants API.

- [ ] **Endpoints de Asistentes & Hilos**
  - [ ] `POST /v1/assistants`, `GET /v1/assistants/{id}`.
  - [ ] `POST /v1/threads`, `POST /v1/threads/{id}/messages`.
  - [ ] `POST /v1/threads/{id}/runs` con ciclo de polling o streaming.
- [ ] **Motor de Ejecución de Runs**
  - [ ] Máquina de estados para gestionar el ciclo de vida del Run (`queued` $\to$ `in_progress` $\to$ `requires_action` $\to$ `completed`).

---

### 🛡️ P6: Guardrails, Evals & Filtrado de Contenido

*Objetivo:* Seguridad de capa de proxy y evaluación continua de calidad.

- [ ] **Guardrails & Filtros de Seguridad**
  - [ ] Detección de prompt injections y jailbreaks en requests entrantes.
  - [ ] Enmascaramiento / Redacción de PII (Personally Identifiable Information) configurable en prompts y outputs.
- [ ] **Evaluaciones y Benchmarking en Producción**
  - [ ] Shadow traffic / Shadow evaluation (ejecución en paralelo silenciosa contra un modelo de control).
  - [ ] A/B Testing controlado por pesos porcentuales en combos de modelos.

---

### 🖥️ P7: Aplicación de Escritorio & Empaquetado Nativo

*Objetivo:* Proveer una experiencia standalone con interfaz nativa para desarrolladores en estaciones de trabajo locales.

- [ ] **Empaquetado de Escritorio**
  - [ ] Integración con Tauri para proveer un binario liviano multiplataforma (Linux, macOS, Windows).
  - [ ] System Tray con control de inicio/parada, logs rápidos y estado de salud.
- [ ] **Instaladores y Paquetes de Sistema**
  - [ ] Homebrew formula para macOS/Linux.
  - [ ] Paquetes Debian (`.deb`) / Arch (`PKGBUILD`) / Windows (`winget`, `.msi`).

---

## 3. Registro de Control de Cambios

- **2026-08-15**: Creación inicial del roadmap post-MVP con priorización de compresión HTTP (`gzip`/`br`), MCP y escalabilidad distribuida.
