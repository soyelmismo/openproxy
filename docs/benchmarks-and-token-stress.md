# OpenProxy Benchmarks & Token Stress Testing Guide

Este documento describe la suite de pruebas de rendimiento y estrés de tokens para OpenProxy.

## Aislamiento de CI (Por qué son opcionales)
Ninguno de estos benchmarks o pruebas de estrés se ejecuta durante el `cargo test` estándar de CI:
- Los benchmarks usan el arnés de **Criterion** (`cargo bench`), que `cargo test` ignora por defecto.
- El ejecutable de estrés de extremo a extremo vive como un **example** (`--example bench_token_stress`) en `openproxy-server`, por lo que tampoco bloquea la suite de tests unitarios/integración.

---

## 1. Nivel 1: Benchmark en Memoria (Throughput Puro de Tokens)
Mide la capacidad teórica del pipeline de parsing y traducción de eventos SSE en memoria a gran escala (1k, 10k, 50k tokens sintéticos por stream) para OpenAI, Anthropic y Gemini, además de acumulación de contexto grande.

### Ejecución rápida:
```bash
# Probar parsing y traducción de tokens de OpenAI a escala
cargo bench --bench token_throughput -- --sample-size 10 openai_token_throughput

# Probar throughput con Anthropic SSE
cargo bench --bench token_throughput -- --sample-size 10 anthropic_token_throughput

# Probar throughput con Gemini SSE
cargo bench --bench token_throughput -- --sample-size 10 gemini_token_throughput

# Medir acumulación de respuestas de hasta 20k tokens en memoria
cargo bench --bench token_throughput -- --sample-size 10 accumulator_token_scale
```

---

## 2. Nivel 2: Benchmark End-to-End con Perfilado de Recursos

El ejecutable de estrés (`bench_token_stress`) instrumenta mediciones del kernel de Linux (`/proc/self/stat`, `/proc/self/status`, `/proc/self/io`) para obtener un desglose a nivel bit:

1. **Latencia por Fase (en microsegundos $\mu s$):**
   - **Dispatch (Routing + Auth + Upstream Connect):** Overhead antes de iniciar el streaming.
   - **TTFT (Time To First Token):** Latencia real percibida por el cliente hasta el primer chunk.
   - **Stream Processing & Drain:** Tiempo total de vaciado del canal SSE.
   - **Average Time Per SSE Chunk:** Coste en $\mu s$ por cada chunk procesado por la lógica del proxy.
2. **Consumo de Memoria:**
   - **Process Resident Set (RSS):** Tamaño residente total del proceso en RAM.
   - **RSS Growth:** Incremento neto de memoria durante el lote de peticiones.
   - **Memory per Request:** Consumo de RAM promedio generado por cada conexión de cliente.
3. **Consumo de CPU:**
   - Ticks de CPU en espacio de usuario (`utime`) y sistema (`stime`).
   - Porcentaje efectivo de utilización multi-núcleo.
4. **I/O Físico y Lógico:**
   - I/O físico a disco (`read_bytes`, `write_bytes` por fsync/WAL).
   - Syscalls lógicos (`rchar`, `wchar`).
   - Consultas a la base de datos en el hot-path (validando la caché en memoria).

### Compilación y Ejecución:

```bash
cargo build --release -p openproxy-server --example bench_token_stress

# Formato:
./target/release/examples/bench_token_stress [CONCURRENCY] [TOKENS_PER_STREAM] [CHUNK_SIZE]

# Ejemplo 1: 1 stream largo de 10.000 tokens
./target/release/examples/bench_token_stress 1 10000 5

# Ejemplo 2: 50 streams concurrentes de 10.000 tokens (500.000 tokens en paralelo)
./target/release/examples/bench_token_stress 50 10000 5
```
