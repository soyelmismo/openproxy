//! Public pricing metadata for the models explicitly advertised by a plan.
//! Estimates are computed by the browser from the credited balance. This is
//! deliberately read-only: it neither creates an order nor touches the ledger.
use openproxy_db::pricing::{self, Price, PriceKind};
use openproxy_types::{Model, Result, combos::Combo};
use rusqlite::Connection;
use serde::Serialize;

#[derive(Debug, Serialize)]
pub(super) struct ModelPricing {
    name: String,
    combo: bool,
    pricing_input_per_1m: Option<f64>,
    pricing_output_per_1m: Option<f64>,
    pricing_cached_per_1m: Option<f64>,
    pricing_schedule: Option<serde_json::Value>,
}

/// One snapshot per request, not one catalog scan per badge or plan.
pub(super) struct PlanCatalog {
    combos: Vec<Combo>,
    models: Vec<Model>,
}

impl PlanCatalog {
    pub(super) fn load(conn: &Connection) -> Result<Self> {
        Ok(Self {
            combos: openproxy_db::combos::list_combos(conn)?,
            models: openproxy_db::models::list_active_all(conn)?,
        })
    }

    pub(super) fn prices(
        &self,
        conn: &Connection,
        models: &[String],
        combos: &[String],
    ) -> Result<Vec<ModelPricing>> {
        models
            .iter()
            .map(|name| self.entry(conn, name, false))
            .chain(
                combos.iter().map(|name| {
                    self.entry(conn, name.strip_prefix("combo:").unwrap_or(name), true)
                }),
            )
            .collect()
    }

    fn entry(&self, conn: &Connection, name: &str, combo: bool) -> Result<ModelPricing> {
        let bare = name.strip_prefix("combo:").unwrap_or(name);
        // Bare combo names are valid in allowed_models too. Display metadata
        // follows the public routing name rather than a guessed upstream rate.
        let matched_combo = self.combos.iter().find(|c| c.name == bare);
        let price = if let Some(c) = matched_combo {
            pricing::lookup_combo_price(conn, c.id.0)
        } else if combo || name.starts_with("combo:") {
            None
        } else {
            self.model_price(conn, name)
        }
        .filter(valid_token_price);
        let schedule = matched_combo
            .and_then(|c| c.pricing_schedule.as_ref())
            .map(serde_json::to_value)
            .transpose()
            .map_err(|err| openproxy_types::CoreError::Internal(err.to_string()))?;
        Ok(ModelPricing {
            name: name.to_string(),
            combo,
            pricing_input_per_1m: price.map(|p| p.input_per_1m),
            pricing_output_per_1m: price.map(|p| p.output_per_1m),
            pricing_cached_per_1m: price.and_then(|p| p.cached_per_1m),
            pricing_schedule: schedule,
        })
    }

    fn model_price(&self, conn: &Connection, name: &str) -> Option<Price> {
        // A wildcard or a provider-less name may match different priced
        // targets. Never advertise the cheapest one as a guaranteed rate.
        let mut prices = self.models.iter().filter_map(|m| {
            let full = format!("{}/{}", m.provider_id, m.model_id);
            (m.model_id.as_str() == name || full == name)
                .then(|| pricing::lookup_with_db(conn, m.provider_id.as_str(), m.model_id.as_str()))
        });
        let first = prices.next()??;
        for price in prices {
            let other = price?;
            if first.input_per_1m != other.input_per_1m
                || first.output_per_1m != other.output_per_1m
                || first.cached_per_1m != other.cached_per_1m
                || first.kind != other.kind
            {
                return None;
            }
        }
        Some(first)
    }
}

fn valid_token_price(p: &Price) -> bool {
    p.kind == PriceKind::Chat
        && [p.input_per_1m, p.output_per_1m]
            .into_iter()
            .chain(p.cached_per_1m)
            .all(|v| v.is_finite() && v >= 0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_valid_token_prices_can_be_used_for_estimates() {
        let mut p = Price {
            input_per_1m: 0.3,
            output_per_1m: 1.2,
            cached_per_1m: Some(0.0),
            ..Default::default()
        };
        assert!(valid_token_price(&p));
        p.cached_per_1m = None;
        assert!(valid_token_price(&p));
        p.kind = PriceKind::Audio;
        assert!(!valid_token_price(&p));
        p.kind = PriceKind::Chat;
        for invalid in [f64::NAN, f64::INFINITY, -0.01] {
            p.cached_per_1m = Some(invalid);
            assert!(!valid_token_price(&p));
            p.cached_per_1m = None;
            p.input_per_1m = invalid;
            assert!(!valid_token_price(&p));
            p.input_per_1m = 0.3;
            p.output_per_1m = invalid;
            assert!(!valid_token_price(&p));
            p.output_per_1m = 1.2;
        }
    }

    #[test]
    fn providerless_models_require_identical_rates_across_every_target() {
        let pool = openproxy_db::conn::DbPool::test_pool_with_prefix("plan-pricing-models")
            .expect("test pool");
        let conn = pool.writer();
        for provider in ["plan-a", "plan-b"] {
            conn.execute("INSERT INTO providers (id, name, base_url, auth_type, format) VALUES (?1, ?1, 'https://example.com', 'bearer', 'openai')", [provider]).unwrap();
            openproxy_db::models::create_custom(
                &conn,
                &openproxy_types::ProviderId::new(provider),
                &openproxy_types::ModelId::new("plan-model"),
                None,
                openproxy_types::TargetFormat::Openai,
                0,
                None,
            )
            .unwrap();
            conn.execute("INSERT INTO model_capabilities_sync (provider_id, model_id, pricing_input_per_1m, pricing_output_per_1m, pricing_cached_per_1m) VALUES (?1, 'plan-model', 0.3, 1.2, 0.0)", [provider]).unwrap();
        }
        let catalog = PlanCatalog::load(&conn).unwrap();
        let price = catalog.model_price(&conn, "plan-model").unwrap();
        assert_eq!(price.input_per_1m, 0.3);
        assert_eq!(price.cached_per_1m, Some(0.0));
        conn.execute("UPDATE model_capabilities_sync SET pricing_input_per_1m=0.6 WHERE provider_id='plan-b'", []).unwrap();
        assert!(
            catalog.model_price(&conn, "plan-model").is_none(),
            "different targets cannot be advertised as the cheapest rate"
        );
        assert_eq!(
            catalog
                .model_price(&conn, "plan-a/plan-model")
                .unwrap()
                .input_per_1m,
            0.3
        );
        assert_eq!(
            catalog
                .model_price(&conn, "plan-b/plan-model")
                .unwrap()
                .input_per_1m,
            0.6
        );
        assert!(catalog.model_price(&conn, "plan-*").is_none());
    }

    #[test]
    fn catalog_preserves_badge_identity_and_never_invents_missing_rates() {
        let conn = Connection::open_in_memory().unwrap();
        // Empty snapshots need no schema or network to prove the unknown path.
        let catalog = PlanCatalog {
            combos: vec![],
            models: vec![],
        };
        let entries = catalog
            .prices(
                &conn,
                &["unknown".into(), "provider/*".into()],
                &["combo:missing".into()],
            )
            .unwrap();
        let value = serde_json::to_value(entries).unwrap();
        assert_eq!(value[0]["name"], "unknown");
        assert_eq!(value[0]["combo"], false);
        assert_eq!(value[2]["name"], "missing");
        assert_eq!(value[2]["combo"], true);
        for entry in value.as_array().unwrap() {
            assert!(entry["pricing_input_per_1m"].is_null());
            assert!(entry["pricing_output_per_1m"].is_null());
            assert!(entry["pricing_schedule"].is_null());
        }
    }
}
