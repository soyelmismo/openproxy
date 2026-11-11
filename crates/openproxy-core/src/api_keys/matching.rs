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

fn is_provider_in_allowlist(allowed: Option<&[String]>, provider: Option<&str>) -> bool {
    let Some(ap) = allowed else {
        return true;
    };
    if ap.is_empty() {
        return true;
    }
    match provider {
        Some(p) => ap.iter().any(|a| a == p || a == "*"),
        None => false,
    }
}

/// Strict-mode gate for `allowed_providers`: a non-empty list grants models
/// from the listed providers (or `*`); an absent or empty list grants
/// nothing — in strict mode an empty allowlist denies everything.
fn is_provider_strictly_allowed(allowed: Option<&[String]>, provider: Option<&str>) -> bool {
    match allowed {
        Some(list) if !list.is_empty() => match provider {
            Some(p) => list.iter().any(|a| a == p || a == "*"),
            None => false,
        },
        _ => false,
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

        let is_model_blacklisted = is_model_blocked_by_blacklist(
            self.blacklisted_models.as_deref(),
            model,
            bare_model,
            full_id.as_deref(),
        );

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

        // Strict mode flips the allowlists into the outer gate: a model is
        // reachable when it matches a non-empty `allowed_models` entry OR its
        // provider matches a non-empty `allowed_providers` entry (either
        // list, when non-empty, grants access; both empty denies everything).
        // Both blacklists keep subtracting from that set.
        if self.only_whitelisted {
            let model_gate = explicitly_allowed;
            let provider_gate =
                is_provider_strictly_allowed(self.allowed_providers.as_deref(), provider_id)
                    || is_provider_strictly_allowed(
                        self.allowed_providers.as_deref(),
                        prov_from_model,
                    );
            if !model_gate && !provider_gate {
                return false;
            }
            let prov_blacklisted =
                is_provider_blacklisted(self.blacklisted_providers.as_deref(), provider_id)
                    || is_provider_blacklisted(
                        self.blacklisted_providers.as_deref(),
                        prov_from_model,
                    );
            return !is_model_blacklisted && !prov_blacklisted;
        }

        // Combos are virtual routing targets, not raw provider models.
        // A combo name (e.g. "combo:nemo3" or "nemo3") has no provider prefix
        // and should not be denied by an allowed_providers whitelist.
        if model.starts_with("combo:") || effective_prov.is_none() {
            if is_model_blacklisted {
                return false;
            }
            // If only_whitelisted is false, combo is allowed
            // (or if only_whitelisted is true, it was already checked above).
            return true;
        }

        if explicitly_allowed {
            return true;
        }

        if is_model_blacklisted {
            return false;
        }

        // Whitelisted providers filter: if allowed_providers is set, only models
        // from those providers are allowed by default (unless explicitly whitelisted above).
        // Blacklisted providers filter: if blacklisted_providers is set, models from
        // those providers are denied.
        let is_blacklisted =
            is_provider_blacklisted(self.blacklisted_providers.as_deref(), provider_id)
                || is_provider_blacklisted(self.blacklisted_providers.as_deref(), prov_from_model);
        if is_blacklisted {
            return false;
        }

        is_provider_in_allowlist(self.allowed_providers.as_deref(), provider_id)
            || is_provider_in_allowlist(self.allowed_providers.as_deref(), prov_from_model)
    }

    /// Returns whether the specified provider is permitted by this key.
    pub fn is_provider_allowed(&self, provider_id: &str) -> bool {
        if let Some(blacklisted) = &self.blacklisted_providers
            && blacklisted.iter().any(|p| p == provider_id || p == "*")
        {
            return false;
        }
        if let Some(allowed) = &self.allowed_providers
            && !allowed.is_empty()
            && !allowed.iter().any(|p| p == provider_id || p == "*")
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
