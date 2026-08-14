---
name: rust-dedup-modernize
description: Skill avanzada de Rust para detectar duplicaciones de código, extraer patrones DRY mediante traits/macros zero-cost y modernizar sintaxis y stdlib (LazyLock, OnceLock, let-else, AFIT/RPITIT, Cow, slice tooling).
---

# Rust Modernization & Code Deduplication Skill

Guía operativa para auditar repositorios Rust, localizar código duplicado/boilerplate y modernizar implementaciones adoptando la stdlib contemporánea (Rust 1.70+ / 2024 edition) con abstracciones zero-cost.

---

## 1. Detección y Deduplicación Sistemática de Código

### 1.1 Patrones de Código Repetitivo Comunes

| Patrón Duplicado | Causa Común | Técnica de Deduplicación Idiomática |
| :--- | :--- | :--- |
| **Handlers / Endpoints HTTP** | Boilerplate repetido de request/response/headers | Macro declarativa (`macro_rules!`) o Middleware / Layer genérico |
| **Transformaciones / Parseo** | Bloques repetidos de validación o normalización | Extension Trait (`trait StrExt { ... } impl StrExt for str`) |
| **Adaptadores de Proveedores / APIs** | Múltiples clientes con endpoints casi idénticos | Trait con métodos default (`trait ProviderAdapter`) |
| **Construcción de Queries SQL** | Inserciones y placeholders repetitivos | Batch helper genérico o macro declarativa de inserción |
| **Conversión de Errores** | Múltiples bloques `.map_err(\|e\| ...)` idénticos | Implementaciones `From<SourceError>` o Extension Trait para `Result` |
| **Normalización de Payloads JSON** | Match arms repetidos en enums de variantes | Visitor pattern o helpers polimórficos (`serde_json::Value`) |

### 1.2 Estrategias de Deduplicación Zero-Cost

1. **Extension Traits (Blanket Implementations):**
   ```rust
   // Centraliza transformaciones comunes sin envolver tipos en nuevos structs
   pub trait ResponseExt {
       fn to_json_response(&self) -> Result<Response, Error>;
   }
   impl<T: serde::Serialize> ResponseExt for T {
       fn to_json_response(&self) -> Result<Response, Error> {
           // ...
       }
   }
   ```

2. **Macros Declarativas (`macro_rules!`):**
   ```rust
   // Deduplica implementaciones repetitivas de traits o adapters
   macro_rules! impl_provider_dispatch {
       ($enum_name:ident, $($variant:ident => $adapter:path),+ $(,)?) => {
           match self {
               $( $enum_name::$variant(inner) => inner.execute(req).await, )+
           }
       };
   }
   ```

3. **Traits con Métodos Default (Polimorfismo Estático):**
   ```rust
   pub trait ApiClient {
       fn base_url(&self) -> &str;
       fn client(&self) -> &reqwest::Client;
       
       // Método por defecto reusable
       fn endpoint_url(&self, path: &str) -> String {
           format!("{}/{}", self.base_url().trim_end_matches('/'), path.trim_start_matches('/'))
       }
   }
   ```

---

## 2. Catálogo de Modernización Idiomática de Rust

### 2.1 Reemplazo de Dependencias por Stdlib (Rust 1.70+)

- **`lazy_static!` / `once_cell::sync::Lazy` -> `std::sync::LazyLock`**
  ```rust
  // ANTES (dependencia externa):
  // static REGEX: Lazy<Regex> = Lazy::new(|| Regex::new("...").unwrap());
  
  // AHORA (stdlib std::sync::LazyLock):
  static REGEX: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
      regex::Regex::new("...").expect("regex compilation failed")
  });
  ```

- **`once_cell::sync::OnceCell` -> `std::sync::OnceLock`**
  ```rust
  // Inicialización única thread-safe nativa
  static CONFIG: std::sync::OnceLock<AppConfig> = std::sync::OnceLock::new();
  let cfg = CONFIG.get_or_init(AppConfig::load);
  ```

- **`atty` / `is-terminal` -> `std::io::IsTerminal`**
  ```rust
  use std::io::IsTerminal;
  if std::io::stdout().is_terminal() {
      // ...
  }
  ```

### 2.2 Control de Flujo Moderno

- **`let-else` para Validación y Salida Temprana (Early Return):**
  ```rust
  // ANTES:
  let value = match opt {
      Some(v) => v,
      None => return Err(Error::NotFound),
  };
  
  // AHORA:
  let Some(value) = opt else {
      return Err(Error::NotFound);
  };
  ```

- **Predicados con `Option::is_some_and` y `Result::is_ok_and`:**
  ```rust
  // ANTES:
  if opt.map_or(false, |x| x > 10) { ... }
  
  // AHORA:
  if opt.is_some_and(|x| x > 10) { ... }
  ```

- **División de Strings con `str::split_once`:**
  ```rust
  // ANTES:
  let parts: Vec<&str> = text.splitn(2, '=').collect();
  if parts.len() == 2 { let (k, v) = (parts[0], parts[1]); }
  
  // AHORA:
  if let Some((k, v)) = text.split_once('=') {
      // ...
  }
  ```

### 2.3 Async y Traits Modernos

- **`async fn in trait` (AFIT) y `impl Trait` en Asociados (RPITIT) (Rust 1.75+):**
  - Evitar el overhead de `#[async_trait]` (`Box<dyn Future>`) en traits internos o de alto rendimiento donde el polimorfismo dinámico no sea mandatorio.
  ```rust
  pub trait MessageConsumer {
      async fn consume(&self, msg: Message) -> Result<(), AppError>;
  }
  ```

- **Zero-Copy y `Cow<'a, T>` en Paths Calientes:**
  ```rust
  use std::borrow::Cow;
  
  pub fn sanitize_prompt<'a>(input: &'a str) -> Cow<'a, str> {
      if input.contains('\0') {
          Cow::Owned(input.replace('\0', ""))
      } else {
          Cow::Borrowed(input) // Cero asignación si no requiere cambios
      }
  }
  ```

- **Scoped Threads (`std::thread::scope`) para Concurrencia Síncrona:**
  ```rust
  // Permite tomar prestadas referencias locales (&T) sin requerir Arc<'static>
  std::thread::scope(|s| {
      s.spawn(|| worker_one(&local_data));
      s.spawn(|| worker_two(&local_data));
  });
  ```

---

## 3. Protocolo de Ejecución de Refactorización

1. **Auditoría de Duplicación y Antipatrones:**
   - Buscar bloques de código sintácticamente idénticos en crates/módulos.
   - Detectar usos de crates obsoletos (`lazy_static`, `atty`, `once_cell`).
   - Identificar anidamientos excesivos sustituibles por `let-else` o combinadores.

2. **Diseño de la Abstracción Central:**
   - Implementar la función base, macro o trait en el módulo base (`common`, `core`, `types`).
   - Reexportar públicamente con `pub use`.

3. **Reemplazo Progresivo:**
   - Reemplazar sitios duplicados por la nueva abstracción.
   - Mantener intactas firmas públicas necesarias para no romper compatibilidad.

4. **Verificación de Calidad:**
   - Ejecutar lints: `cargo clippy --workspace --all-targets -- -D warnings`.
   - Ejecutar tests: `cargo test --workspace`.
