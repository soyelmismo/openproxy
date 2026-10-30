use super::models::ApiKey;

fn matches_model_spec(spec: &str, candidate: &str) -> bool {
    if spec == "*" || spec == candidate {
        return true;
    }
    if let Some(prefix) = spec.strip_suffix('*')
        && candidate.starts_with(prefix)
    {
        return true;
    }
    if let Some(suffix) = spec.strip_prefix('*')
        && candidate.ends_with(suffix)
    {
        return true;
    }
    false
}

fn model_matches_pattern(
    pattern: &str,
    model: &str,
    bare_model: &str,
    full_id: Option<&str>,
) -> bool {
    matches_model_spec(pattern, model)
        || matches_model_spec(pattern, bare_model)
        || full_id.is_some_and(|f| matches_model_spec(pattern, f))
}

fn is_provider_blacklisted(blacklisted: Option<&[String]>, provider: Option<&str>) -> bool {
    if let (Some(bp), Some(p)) = (blacklisted, provider) {
        bp.iter().any(|b| b == p || b == "*")
    } else {
        false
    }
}

fn is_model_blocked_by_blacklist(
    blacklisted_models: Option<&[String]>,
    model: &str,
    bare_model: &str,
    full_id: Option<&str>,
) -> bool {
    let Some(blacklisted) = blacklisted_models else {
        return false;
    };
    blacklisted
        .iter()
        .any(|b| model_matches_pattern(b, model, bare_model, full_id))
}

impl ApiKey {
    /// Returns whether the specified model is permitted by this key's
    /// `allowed_models`, `blacklisted_models`, and `blacklisted_providers`.
    pub fn is_model_allowed(&self, model: &str, provider_id: Option<&str>) -> bool {
        let (prov_from_model, bare_model) = model
            .split_once('/')
            .map_or((None, model), |(p, m)| (Some(p), m));
        let effective_prov = provider_id.or(prov_from_model);
        let full_id = effective_prov.and_then(|p| {
            if model.starts_with(&format!("{p}/")) {
                None
            } else {
                Some(format!("{p}/{model}"))
            }
        });

        // `allowed_models` is ADDITIVE, not a whitelist: it grants access on
        // top of the default-permit, and an entry overrides BOTH the model and
        // the provider blacklists (e.g. open `devin/swe-2:free` while `devin`
        // stays provider-blocked). Models not in the list are still allowed
        // unless a blacklist matches them.
        let explicitly_allowed = self.allowed_models.as_deref().is_some_and(|allowed| {
            !allowed.is_empty()
                && allowed
                    .iter()
                    .any(|m| model_matches_pattern(m, model, bare_model, full_id.as_deref()))
        });
        if explicitly_allowed {
            return true;
        }

        if is_model_blocked_by_blacklist(
            self.blacklisted_models.as_deref(),
            model,
            bare_model,
            full_id.as_deref(),
        ) {
            return false;
        }

        !is_provider_blacklisted(self.blacklisted_providers.as_deref(), provider_id)
            && !is_provider_blacklisted(self.blacklisted_providers.as_deref(), prov_from_model)
    }

    /// Returns whether the specified provider is permitted by this key.
    pub fn is_provider_allowed(&self, provider_id: &str) -> bool {
        if let Some(blacklisted) = &self.blacklisted_providers
            && blacklisted.iter().any(|p| p == provider_id || p == "*")
        {
            return false;
        }
        true
    }

    /// Returns whether the specified combo ID is permitted by this key.
    pub fn is_combo_allowed(&self, combo_id: i64) -> bool {
        if let Some(allowed) = &self.allowed_combos
            && !allowed.is_empty()
            && !allowed.contains(&combo_id)
        {
            return false;
        }
        true
    }
}
