use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::{
    Acquired, CacheEntry, ContractError, ContractLimits, ContractTransport, FetchRequest,
    FetchResponse, SourceDescriptor, TransportCache, TransportCacheKey, hex_digest,
    validate_source_body_size,
};

/// Infraestructura compartida de un driver de release: el transporte, la cache
/// y los límites del base.
///
/// [`CodexReleaseDriver`] y
/// [`super::super::codebuddy_runtime::CodeBuddyReleaseDriver`] llevan el mismo
/// trío de estado y fabrican motores de adquisición nuevos por ronda: el
/// contador de bytes frescos es propiedad de la ronda, no del driver.
pub(crate) struct ReleaseInfra {
    transport: Arc<dyn ContractTransport>,
    cache: Arc<TransportCache>,
    limits: ContractLimits,
}

impl ReleaseInfra {
    /// Construye la infraestructura compartida.
    pub(crate) fn new(
        transport: Arc<dyn ContractTransport>,
        cache: Arc<TransportCache>,
        limits: ContractLimits,
    ) -> Self {
        Self {
            transport,
            cache,
            limits,
        }
    }

    /// La cache compartida, para inspección o precalentamiento.
    pub(crate) fn cache(&self) -> &Arc<TransportCache> {
        &self.cache
    }

    /// Los límites de la ronda.
    pub(crate) fn limits(&self) -> &ContractLimits {
        &self.limits
    }

    /// Un motor de adquisición nuevo sobre la misma infraestructura: su
    /// contador de bytes frescos arranca en cero para cada ronda.
    pub(crate) fn acquirer_core(&self) -> AcquirerCore {
        AcquirerCore::new(
            Arc::clone(&self.transport),
            Arc::clone(&self.cache),
            self.limits,
        )
    }
}

/// Motor compartido de adquisición condicional sobre la infraestructura del
/// base.
///
/// Reutiliza la cache, los límites y la semántica condicional del base: una
/// petición por recurso, `304` servido desde los bytes cacheados, `404`/`410`/
/// `429`/`5xx` mapeados a los errores del base. No hay un segundo GET de
/// revalidación ni un fetch duplicado del mismo recurso.
///
/// La unidad devuelta la construye el llamador con `make_unit`: el motor solo
/// se ocupa del transporte, la cache y la contabilidad de frescura, así que la
/// misma mecánica sirve a [`CodexReleaseAcquirer`] y a
/// [`super::super::codebuddy_runtime::CodeBuddyReleaseAcquirer`], cuyas
/// unidades `Acquired` difieren solo en la forma del tipo.
pub(crate) struct AcquirerCore {
    transport: Arc<dyn ContractTransport>,
    cache: Arc<TransportCache>,
    limits: ContractLimits,
    fresh: AtomicUsize,
}

impl AcquirerCore {
    /// Construye el motor sobre infraestructura compartida.
    pub(crate) fn new(
        transport: Arc<dyn ContractTransport>,
        cache: Arc<TransportCache>,
        limits: ContractLimits,
    ) -> Self {
        Self {
            transport,
            cache,
            limits,
            fresh: AtomicUsize::new(0),
        }
    }

    /// La cache compartida, para inspección o precalentamiento.
    pub(crate) fn cache(&self) -> &Arc<TransportCache> {
        &self.cache
    }

    /// Cuántos recursos devolvieron bytes frescos en esta ronda.
    pub(crate) fn fresh_count(&self) -> usize {
        self.fresh.load(Ordering::Relaxed)
    }

    /// Adquiere un recurso siguiendo la semántica condicional del base.
    ///
    /// Devuelve siempre bytes utilizables (frescos o cacheados), pasados por
    /// `make_unit` con el descriptor, el cuerpo y el digest de la unidad. Un
    /// `304` sin bytes cacheados es error real, y un cuerpo byte-idéntico al
    /// cacheado no cuenta como nuevo.
    pub(crate) async fn acquire<A>(
        &self,
        descriptor: &SourceDescriptor,
        make_unit: impl Fn(&SourceDescriptor, Vec<u8>, String) -> A,
    ) -> Result<A, ContractError> {
        let key =
            TransportCacheKey::new(&descriptor.url, &descriptor.accept, &descriptor.auth_scope);
        let cached = self.cache.get(&key);
        let request = self.build_request(descriptor, cached.as_ref());
        let response = self.transport.fetch(&request).await?;

        match response.status {
            200..=299 => self.handle_ok(descriptor, &key, cached, &response, &make_unit),
            304 => {
                self.handle_not_modified(descriptor, cached, &response, &make_unit)
                    .await
            }
            status => Err(status_error(&descriptor.url, status, &response)),
        }
    }

    /// Petición de un paso, con los validadores cacheados si son reusables.
    fn build_request(
        &self,
        descriptor: &SourceDescriptor,
        cached: Option<&CacheEntry>,
    ) -> FetchRequest {
        let mut headers = descriptor.headers.clone();
        headers.push(("Accept".to_string(), descriptor.accept.clone()));
        if let Some(entry) = cached.filter(|entry| entry.is_replayable()) {
            headers.extend(entry.conditional_headers());
        }
        FetchRequest {
            key: TransportCacheKey::new(
                &descriptor.url,
                &descriptor.accept,
                &descriptor.auth_scope,
            ),
            method: "GET".to_string(),
            url: descriptor.url.clone(),
            headers,
        }
    }

    /// Registra los bytes y validadores frescos para la próxima condicional.
    fn record(&self, key: &TransportCacheKey, response: &FetchResponse, digest: String) {
        self.cache.insert(CacheEntry {
            key: key.clone(),
            etag: response.etag(),
            last_modified: response.last_modified(),
            checked_at: chrono::Utc::now(),
            digest,
            bytes: response.body.len(),
            body: Some(response.body.clone()),
        });
    }

    /// Refresca los validadores de una entrada cacheada, conservando sus bytes.
    fn touch(&self, entry: CacheEntry, response: &FetchResponse) {
        let mut updated = entry;
        if let Some(etag) = response.etag() {
            updated.etag = Some(etag);
        }
        if let Some(last_modified) = response.last_modified() {
            updated.last_modified = Some(last_modified);
        }
        updated.checked_at = chrono::Utc::now();
        self.cache.insert(updated);
    }

    /// `2xx`: cuerpo fresco, o byte-idéntico al cacheado.
    fn handle_ok<A>(
        &self,
        descriptor: &SourceDescriptor,
        key: &TransportCacheKey,
        cached: Option<CacheEntry>,
        response: &FetchResponse,
        make_unit: &impl Fn(&SourceDescriptor, Vec<u8>, String) -> A,
    ) -> Result<A, ContractError> {
        validate_source_body_size(response.body.len(), &self.limits)?;
        let digest = hex_digest(&response.body);
        if let Some(entry) =
            cached.filter(|entry| entry.body_matches_digest() && entry.digest == digest)
        {
            // Mismo contenido, validadores nuevos: los bytes cacheados sirven.
            // La unidad se construye antes de mover la entrada al `touch`.
            let unit = make_unit(
                descriptor,
                entry.body.clone().unwrap_or_default(),
                entry.digest.clone(),
            );
            self.touch(entry, response);
            return Ok(unit);
        }
        self.record(key, response, digest.clone());
        self.fresh.fetch_add(1, Ordering::Relaxed);
        Ok(make_unit(descriptor, response.body.clone(), digest))
    }

    /// `304`: solo sirve si los bytes cacheados siguen intactos.
    async fn handle_not_modified<A>(
        &self,
        descriptor: &SourceDescriptor,
        cached: Option<CacheEntry>,
        response: &FetchResponse,
        make_unit: &impl Fn(&SourceDescriptor, Vec<u8>, String) -> A,
    ) -> Result<A, ContractError> {
        if let Some(entry) = cached.filter(CacheEntry::body_matches_digest) {
            // La unidad se construye antes de mover la entrada al `touch`.
            let unit = make_unit(
                descriptor,
                entry.body.clone().unwrap_or_default(),
                entry.digest.clone(),
            );
            self.touch(entry, response);
            return Ok(unit);
        }
        // Sin bytes usables un `304` no prueba nada: un reintento
        // incondicional, y un segundo `304` se rechaza.
        let retry = self.build_request(descriptor, None);
        let retried = self.transport.fetch(&retry).await?;
        match retried.status {
            200..=299 => self.handle_ok(descriptor, &retry.key, None, &retried, make_unit),
            304 => Err(ContractError::MissingCachedBytes {
                url: descriptor.url.clone(),
            }),
            status => Err(status_error(&descriptor.url, status, &retried)),
        }
    }
}

/// Adquisición de un recurso sobre el transporte compartido.
///
/// Reutiliza la cache, los límites y la semántica condicional del base: una
/// petición por recurso, `304` servido desde los bytes cacheados, `404`/`410`/
/// `429`/`5xx` mapeados a los errores del base. No hay un segundo GET de
/// revalidación ni un fetch duplicado del mismo recurso.
pub struct CodexReleaseAcquirer {
    inner: AcquirerCore,
}

impl CodexReleaseAcquirer {
    /// Construye un adquisidor sobre infraestructura compartida.
    pub fn new(
        transport: Arc<dyn ContractTransport>,
        cache: Arc<TransportCache>,
        limits: ContractLimits,
    ) -> Self {
        Self {
            inner: AcquirerCore::new(transport, cache, limits),
        }
    }

    /// La cache compartida, para inspección o precalentamiento.
    pub fn cache(&self) -> &Arc<TransportCache> {
        self.inner.cache()
    }

    /// Envuelve un motor de la ronda en el adquisidor tipado.
    pub(super) fn from_core(inner: AcquirerCore) -> Self {
        Self { inner }
    }

    /// Cuántos recursos devolvieron bytes frescos en esta ronda.
    pub fn fresh_count(&self) -> usize {
        self.inner.fresh_count()
    }

    /// Adquiere un recurso siguiendo la semántica condicional del base.
    ///
    /// Devuelve siempre bytes utilizables (frescos o cacheados); `Acquired::fresh`
    /// distingue el caso. Un `304` sin bytes cacheados es error real, y un
    /// cuerpo byte-idéntico al cacheado no cuenta como nuevo.
    pub(super) async fn acquire(
        &self,
        descriptor: &SourceDescriptor,
    ) -> Result<Acquired, ContractError> {
        self.inner
            .acquire(descriptor, |descriptor, body, digest| Acquired {
                url: descriptor.url.clone(),
                body,
                digest,
                revision: descriptor.revision.clone(),
                evidence: descriptor.evidence,
            })
            .await
    }
}

/// Mapea un estado no-2xx, no-304 al error del base.
fn status_error(url: &str, status: u16, response: &FetchResponse) -> ContractError {
    match status {
        404 | 410 => ContractError::SourceMissing {
            url: url.to_string(),
        },
        429 => ContractError::RateLimited {
            url: url.to_string(),
            retry_after: response.retry_after_secs(),
        },
        status if (500..=599).contains(&status) => ContractError::SourceUnavailable {
            url: url.to_string(),
            status,
        },
        status => ContractError::UnexpectedStatus {
            url: url.to_string(),
            status,
        },
    }
}
