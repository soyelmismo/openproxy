//! PII candidate collection and entity match orchestration.

pub mod network_finance;
pub mod person;
pub mod secrets;

use openproxy_types::config::PiiEntity;
use std::ops::Range;

use network_finance::collect_network_finance_candidates;
use person::collect_person_candidates;
use secrets::collect_secret_candidates;

/// A raw detected PII match candidate before overlap resolution.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Candidate<'a> {
    pub(crate) start: usize,
    pub(crate) end: usize,
    pub(crate) entity: PiiEntity,
    pub(crate) matched_text: &'a str,
}

/// Collect all raw candidates across enabled entity types.
pub(crate) fn collect_candidates<'a>(
    entities: &[PiiEntity],
    text: &'a str,
    syntax_protected: &[Range<usize>],
    code_protected: &[Range<usize>],
) -> Vec<Candidate<'a>> {
    let mut candidates = Vec::new();

    if entities.contains(&PiiEntity::Secret) {
        collect_secret_candidates(text, syntax_protected, &mut candidates);
    }

    if entities.contains(&PiiEntity::Email)
        || entities.contains(&PiiEntity::CreditCard)
        || entities.contains(&PiiEntity::Ip)
        || entities.contains(&PiiEntity::Phone)
    {
        collect_network_finance_candidates(entities, text, syntax_protected, &mut candidates);
    }

    if entities.contains(&PiiEntity::Person) {
        collect_person_candidates(text, syntax_protected, code_protected, &mut candidates);
    }

    candidates
}
