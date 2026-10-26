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

## 2. Nivel 2: Benchmark End-to-End (Estrés Completo de Servidor)
Levanta una instancia real de OpenProxy (Axum router + autenticación + DB SQLite en memoria temporal + pool de adapters + middleware) conectada a un servidor mock upstream SSE ultrarrápido que emite streams sin latencia de red de internet.

Mide el rendimiento real de entrega HTTP, multiplexación asíncrona de Tokio, parsing y consumo de chunks SSE bajo concurrencia pesada.

### Ubicación del ejecutable:
`crates/openproxy-server/examples/bench_token_stress.rs`

### Compilación optimizada:
```bash
cargo build --release -p openproxy-server --example bench_token_stress
```

### Parámetros:
```bash
./target/release/examples/bench_token_stress [CONCURRENCY] [TOKENS_PER_STREAM] [CHUNK_SIZE]
```
- `CONCURRENCY`: Número de clientes simultáneos enviando peticiones en paralelo (ej. 10, 50, 100).
- `TOKENS_PER_STREAM`: Cantidad total de tokens que emitirá cada stream (ej. 1,000, 10,000, 50,000).
- `CHUNK_SIZE`: Cuántos tokens viajan en cada frame SSE individual (por defecto 5).

### Ejemplos de uso:

1. **Prueba rápida (10 streams de 5k tokens = 50,000 tokens):**
   ```bash
   ./target/release/examples/bench_token_stress 10 5000 5
   ```

2. **Carga media (50 clientes de 10k tokens = 500,000 tokens):**
   ```bash
   ./target/release/examples/bench_token_stress 50 10000 5
   ```

3. **Carga extrema (100 clientes de 20k tokens = 2,000,000 tokens):**
   ```bash
   ./target/release/examples/bench_token_stress 100 20000 10
   ```

### Métricas reportadas:
- **Elapsed Time:** Tiempo total de ejecución de la prueba.
- **Successful Requests:** Peticiones HTTP completadas exitosamente sin errores ni desconexiones.
- **Total Tokens Delivered:** Conteo real de tokens transferidos a través del proxy.
- **Throughput (tokens/sec):** Velocidad efectiva de tokens procesados por segundo.
- **Data Transfer:** Megabytes totales transferidos y tasa de transferencia en MB/s.
- **SSE Chunks Processed:** Número total de eventos SSE leídos y ensamblados.
- **Avg Stream Time:** Latencia promedio por stream individual.
