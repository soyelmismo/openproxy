use serde_json::Value;

use super::{ContractError, is_json_body, parse_version};
pub struct RegistryMetadata {
    /// Versión anunciada por `latest`.
    pub version: String,
    /// `dist.integrity` declarada por el registro, si existe.
    pub integrity: Option<String>,
}

/// Parsea la metadata mutable del registro.
///
/// # Errors
/// Rechaza un cuerpo que no sea JSON, sin `version`, o con una versión no
/// semver de tres partes.
pub fn parse_registry_metadata(body: &[u8]) -> Result<RegistryMetadata, ContractError> {
    if !is_json_body(body) {
        return Err(ContractError::Rejected(
            "registry/latest no devolvió JSON".to_string(),
        ));
    }
    let value: Value = serde_json::from_slice(body).map_err(ContractError::from)?;
    let version = value
        .get("version")
        .and_then(Value::as_str)
        .ok_or_else(|| ContractError::Rejected("registry/latest sin campo `version`".to_string()))?
        .to_string();
    parse_version(&version)?;
    let integrity = value
        .get("dist")
        .and_then(|dist| dist.get("integrity"))
        .and_then(Value::as_str)
        .map(str::to_string);
    Ok(RegistryMetadata { version, integrity })
}

/// `package.json` del paquete fijado, en lo que el contrato usa.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackageManifest {
    /// Nombre del paquete.
    pub name: String,
    /// Versión declarada.
    pub version: String,
}

/// Parsea el `package.json` del paquete.
///
/// # Errors
/// Rechaza un cuerpo que no sea JSON o sin `name`/`version` de texto.
pub fn parse_package_manifest(body: &[u8]) -> Result<PackageManifest, ContractError> {
    if !is_json_body(body) {
        return Err(ContractError::Rejected(
            "package.json no devolvió JSON".to_string(),
        ));
    }
    let value: Value = serde_json::from_slice(body).map_err(ContractError::from)?;
    let name = value
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| ContractError::Rejected("package.json sin campo `name`".to_string()))?
        .to_string();
    let version = value
        .get("version")
        .and_then(Value::as_str)
        .ok_or_else(|| ContractError::Rejected("package.json sin campo `version`".to_string()))?
        .to_string();
    parse_version(&version)?;
    Ok(PackageManifest { name, version })
}

/// Campos de `product.json` que el contrato publica.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProductDocument {
    /// Endpoint de producción.
    pub endpoint: String,
    /// Endpoint de staging.
    pub staging_endpoint: String,
    /// Tipo de despliegue.
    pub deployment_type: String,
    /// Plataforma.
    pub platform: String,
    /// Endpoints oficiales, si el documento los declara como lista de textos.
    pub official_endpoints: Vec<String>,
}

/// Extrae los campos publicados de `product.json`.
///
/// Un campo de texto ausente o de otro tipo se rechaza: el contrato no rellena
/// huecos con valores por defecto.
///
/// # Errors
/// [`ContractError::Rejected`] si falta un campo obligatorio o no es texto.
pub fn parse_product_document(body: &[u8]) -> Result<ProductDocument, ContractError> {
    if !is_json_body(body) {
        return Err(ContractError::Rejected(
            "product.json no devolvió JSON".to_string(),
        ));
    }
    let value: Value = serde_json::from_slice(body).map_err(ContractError::from)?;
    let text = |field: &str| -> Result<String, ContractError> {
        value
            .get(field)
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| {
                ContractError::Rejected(format!("product.json sin campo de texto `{field}`"))
            })
    };
    let official_endpoints = value
        .get("officialEndpoints")
        .and_then(Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Ok(ProductDocument {
        endpoint: text("endpoint")?,
        staging_endpoint: text("stagingEndpoint")?,
        deployment_type: text("deploymentType")?,
        platform: text("platform")?,
        official_endpoints,
    })
}
