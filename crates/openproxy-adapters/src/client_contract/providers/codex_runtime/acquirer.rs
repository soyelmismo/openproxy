use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::{
    Acquired, CacheEntry, ContractError, ContractLimits, ContractTransport, FetchResponse,
    SourceDescriptor, TransportCache, TransportCacheKey, hex_digest, validate_source_body_size,
};
use crate::client_contract::refresh::{
    build_conditional_request, record_cache_entry, status_error, touch_cache_entry,
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
        let request = build_conditional_request(descriptor, cached.as_ref());
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
            let unit = make_unit(
                descriptor,
                entry.body.clone().unwrap_or_default(),
                entry.digest.clone(),
            );
            touch_cache_entry(&self.cache, entry, response);
            return Ok(unit);
        }
        record_cache_entry(&self.cache, key, response, digest.clone());
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
            let unit = make_unit(
                descriptor,
                entry.body.clone().unwrap_or_default(),
                entry.digest.clone(),
            );
            touch_cache_entry(&self.cache, entry, response);
            return Ok(unit);
        }
        let retry = build_conditional_request(descriptor, None);
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


