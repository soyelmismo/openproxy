pub(crate) fn first_header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

pub(crate) fn has_header(headers: &[(String, String)], name: &str) -> bool {
    headers.iter().any(|(k, _)| k == name)
}

mod custom;
mod general;
#[cfg(feature = "laya-engine")]
mod laya_engine_tests;
mod opencode;
mod providers;
