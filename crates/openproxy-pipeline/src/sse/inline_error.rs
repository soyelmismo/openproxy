#[derive(serde::Deserialize)]
struct InlineErrorProbe<'a> {
    #[serde(borrow)]
    choices: Option<Vec<&'a serde_json::value::RawValue>>,
    #[serde(borrow)]
    error: Option<InlineErrorPayload<'a>>,
    #[serde(borrow)]
    provider: Option<&'a str>,
}

#[derive(serde::Deserialize)]
#[serde(untagged)]
enum InlineErrorPayload<'a> {
    Obj(InlineErrorObjProbe<'a>),
    #[serde(borrow)]
    Str(&'a str),
}

#[derive(serde::Deserialize)]
struct InlineErrorObjProbe<'a> {
    #[serde(borrow)]
    code: Option<InlineErrorCodeProbe<'a>>,
    #[serde(borrow)]
    message: Option<&'a str>,
    #[serde(borrow)]
    r#type: Option<&'a str>,
}

#[derive(serde::Deserialize)]
#[serde(untagged)]
enum InlineErrorCodeProbe<'a> {
    Num(u64),
    #[serde(borrow)]
    Str(&'a str),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedInlineError<'a> {
    pub status_code: u16,
    pub message: &'a str,
    pub provider: Option<&'a str>,
}

pub fn parse_inline_sse_error<'a>(json_payload: &'a str) -> Option<ParsedInlineError<'a>> {
    if !json_payload.contains("\"error\"")
        || (json_payload.contains("\"choices\":") && !json_payload.contains("\"choices\":[]"))
    {
        return None;
    }

    let probe = serde_json::from_str::<InlineErrorProbe<'a>>(json_payload).ok()?;
    if !probe.choices.as_ref().is_none_or(std::vec::Vec::is_empty) {
        return None;
    }

    let (status_code, message) = match probe.error? {
        InlineErrorPayload::Str(msg) => (502, msg),
        InlineErrorPayload::Obj(obj) => {
            let msg = obj
                .message
                .unwrap_or("unknown upstream error in SSE stream");
            let mut resolved = match obj.code {
                Some(InlineErrorCodeProbe::Num(n)) if (400..=599).contains(&n) => n as u16,
                Some(InlineErrorCodeProbe::Str(s)) => {
                    if let Ok(n) = s.parse::<u16>()
                        && (400..=599).contains(&n)
                    {
                        n
                    } else {
                        const CODE_PATTERNS: &[(&[&str], u16)] = &[
                            (&["rate_limit", "quota", "resource_exhausted"], 429),
                            (
                                &[
                                    "content_filter",
                                    "safety",
                                    "policy",
                                    "blocked",
                                    "invalid",
                                    "bad_request",
                                    "malformed",
                                ],
                                400,
                            ),
                            (&["auth", "unauthorized", "permission", "forbidden"], 403),
                        ];
                        CODE_PATTERNS
                            .iter()
                            .find_map(|(kws, code)| {
                                kws.iter().any(|k| s.contains(k)).then_some(*code)
                            })
                            .unwrap_or(502)
                    }
                }
                _ => 502,
            };

            if resolved == 502
                && let Some(t) = obj.r#type
            {
                const TYPE_PATTERNS: &[(&[&str], u16)] = &[
                    (&["rate_limit"], 429),
                    (&["content_filter", "safety", "invalid_request"], 400),
                    (&["auth"], 401),
                ];
                if let Some(code) = TYPE_PATTERNS
                    .iter()
                    .find_map(|(kws, code)| kws.iter().any(|k| t.contains(k)).then_some(*code))
                {
                    resolved = code;
                }
            }

            (resolved, msg)
        }
    };

    Some(ParsedInlineError {
        status_code,
        message,
        provider: probe.provider,
    })
}
