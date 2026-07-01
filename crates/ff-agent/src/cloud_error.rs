//! Per-vendor classification of cloud-CLI / LLM-API error output into a
//! vendor-agnostic [`CloudErrorClass`], plus the recommended [`ErrorAction`]
//! per class.
//!
//! ForgeFleet dispatches headless agent work to cloud CLIs (claude, codex,
//! kimi, gemini, grok) and to local fleet models. When one of those errors
//! mid-task — a `529 Overloaded`, a `429`, an expired token — a headless
//! session has **no human to type "continue"**, so we must classify the
//! failure and act on it programmatically: retry/auto-continue the transient
//! ones, fail over to another provider on the persistent ones.
//!
//! The codes do NOT line up across vendors (claude overload = `529`,
//! OpenAI/Gemini = `503`; `429` splits into rate-limit vs quota only via the
//! `error.type`/message text). So classification is per-vendor and works off
//! the CLI's combined stdout+stderr text plus its exit status — these are
//! subprocesses, not direct HTTP calls.
//!
//! Design: `plans/cloud-error-handling.md` (Layers 1–2).

/// Vendor-agnostic class of a cloud-provider error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloudErrorClass {
    /// Server temporarily overloaded (claude 529, openai/gemini 503,
    /// kimi 429 overload subtype). Transient — back off and continue.
    Overloaded,
    /// Rate limit hit (requests/tokens per minute). Transient — honor
    /// Retry-After, back off and continue.
    RateLimited,
    /// Subscription quota / credit balance exhausted (openai
    /// insufficient_quota, kimi exceeded_current_quota, grok 402, gemini
    /// RPD). NOT retryable on this backend — switch + alert.
    QuotaExhausted,
    /// Auth failed / token expired (401). Flip the backend to
    /// unauthenticated, switch, alert the operator to re-auth.
    Unauthenticated,
    /// Permission / geo / precondition (403, gemini 400 FAILED_PRECONDITION).
    /// Terminal for this backend — switch + alert.
    Forbidden,
    /// Prompt exceeds the context window (claude 413, openai
    /// context_length_exceeded). Not retryable as-is — compact then continue.
    ContextTooLong,
    /// Request timed out / deadline exceeded (gemini 504, stream stall).
    /// Retry once then switch.
    Timeout,
    /// Generic upstream 5xx that is not an overload (500 internal). Transient.
    Transient5xx,
    /// Model id unknown / deprecated / no access (404). Switch model/backend.
    ModelNotFound,
    /// Malformed/invalid request (400 invalid_request, 422). Our bug —
    /// terminal, do not blind-retry.
    BadRequest,
    /// Output blocked by a content/safety filter. Terminal for this prompt.
    ContentFiltered,
    /// Connection refused / DNS / TLS / socket reset. Transient — back off.
    Network,
    /// Recognized as a failure but not matched to a class. Conservative:
    /// one retry then switch; log raw output to grow the taxonomy.
    Unknown,
}

/// What the dispatcher should do about a [`CloudErrorClass`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorAction {
    /// Transient: exponential backoff + auto-continue the same session, up to
    /// `max_attempts`, then escalate to [`Self::SwitchProvider`].
    RetryBackoff { max_attempts: u32 },
    /// Fail over to the next provider in rank/headroom order now.
    SwitchProvider,
    /// Fail over AND alert the operator (quota/permission needs attention).
    SwitchProviderAlert,
    /// Mark this backend `authenticated=false`, then switch + alert (re-auth).
    FlipAuthThenSwitch,
    /// Compact/trim the context, then continue on the same backend.
    CompactThenContinue,
    /// Do not retry — surface to caller (our bug or content policy).
    Terminal,
}

/// Raw cloud-provider failure details suitable for classification.
#[derive(Debug, Clone)]
pub struct CloudError {
    pub provider: String,
    pub exit_code: Option<i32>,
    pub output: String,
}

impl std::fmt::Display for CloudError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.exit_code {
            Some(code) => write!(f, "{} cloud error (exit {code}): {}", self.provider, self.output),
            None => write!(f, "{} cloud error: {}", self.provider, self.output),
        }
    }
}

impl std::error::Error for CloudError {}

/// Operator-facing classification summary for a cloud-provider failure.
#[derive(Debug, Clone)]
pub struct ClassifiedCloudError {
    pub class: CloudErrorClass,
    pub action: ErrorAction,
    pub friendly_message: String,
}

impl CloudErrorClass {
    /// Whether this class is worth retrying on the *same* backend before
    /// considering a provider switch.
    pub fn is_transient(self) -> bool {
        matches!(
            self,
            Self::Overloaded
                | Self::RateLimited
                | Self::Transient5xx
                | Self::Timeout
                | Self::Network
        )
    }

    /// The recommended action. Numbers reflect the ff-council (codex+kimi)
    /// consensus — retries short, fail over to another provider quickly — and
    /// may be overridden from config.
    pub fn action(self) -> ErrorAction {
        match self {
            // Transient — back off + auto-continue, then switch if it persists.
            // Council consensus: short retry budget, then provider switch.
            Self::Overloaded | Self::RateLimited | Self::Transient5xx => {
                ErrorAction::RetryBackoff { max_attempts: 3 }
            }
            Self::Network => ErrorAction::RetryBackoff { max_attempts: 3 },
            Self::Timeout => ErrorAction::RetryBackoff { max_attempts: 2 },
            // Persistent on this backend — switch.
            Self::QuotaExhausted | Self::Forbidden => ErrorAction::SwitchProviderAlert,
            Self::Unauthenticated => ErrorAction::FlipAuthThenSwitch,
            Self::ModelNotFound => ErrorAction::SwitchProvider,
            Self::ContextTooLong => ErrorAction::CompactThenContinue,
            // Our bug / policy — surface, don't loop.
            Self::BadRequest | Self::ContentFiltered => ErrorAction::Terminal,
            // Be conservative on the unknown.
            Self::Unknown => ErrorAction::RetryBackoff { max_attempts: 1 },
        }
    }

    /// Stable lowercase tag for logging / DB.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Overloaded => "overloaded",
            Self::RateLimited => "rate_limited",
            Self::QuotaExhausted => "quota_exhausted",
            Self::Unauthenticated => "unauthenticated",
            Self::Forbidden => "forbidden",
            Self::ContextTooLong => "context_too_long",
            Self::Timeout => "timeout",
            Self::Transient5xx => "transient_5xx",
            Self::ModelNotFound => "model_not_found",
            Self::BadRequest => "bad_request",
            Self::ContentFiltered => "content_filtered",
            Self::Network => "network",
            Self::Unknown => "unknown",
        }
    }
}

/// Classify a cloud-provider failure and return an operator-friendly status.
pub fn classify_cloud_error(err: &CloudError) -> ClassifiedCloudError {
    let class = classify(&err.provider, err.exit_code, &err.output);
    let action = class.action();
    let friendly_message = match class {
        CloudErrorClass::Overloaded => {
            format!(
                "{} is temporarily overloaded. ForgeFleet will retry or switch providers.",
                err.provider
            )
        }
        CloudErrorClass::RateLimited => {
            format!(
                "{} is rate-limiting requests. ForgeFleet will back off before continuing.",
                err.provider
            )
        }
        CloudErrorClass::QuotaExhausted => {
            format!(
                "{} quota or credits are exhausted. Switch providers or update billing.",
                err.provider
            )
        }
        CloudErrorClass::Unauthenticated => {
            format!(
                "{} is not authenticated. Re-authenticate that CLI before using this backend.",
                err.provider
            )
        }
        CloudErrorClass::Forbidden => {
            format!(
                "{} rejected the request due to permissions or account policy.",
                err.provider
            )
        }
        CloudErrorClass::ContextTooLong => {
            format!(
                "The prompt is too large for {}. Compact or reduce context before retrying.",
                err.provider
            )
        }
        CloudErrorClass::Timeout => {
            format!(
                "{} timed out. ForgeFleet will retry or switch providers if it persists.",
                err.provider
            )
        }
        CloudErrorClass::Transient5xx => {
            format!(
                "{} returned a temporary server error. Retry or switch providers if it persists.",
                err.provider
            )
        }
        CloudErrorClass::ModelNotFound => {
            format!(
                "{} could not find or access the requested model. Pick another model/backend.",
                err.provider
            )
        }
        CloudErrorClass::BadRequest => {
            format!(
                "{} rejected the request as invalid. Check the generated prompt or backend arguments.",
                err.provider
            )
        }
        CloudErrorClass::ContentFiltered => {
            format!("{} blocked the response with a content filter.", err.provider)
        }
        CloudErrorClass::Network => {
            format!("Network connectivity to {} failed. Check connectivity or retry.", err.provider)
        }
        CloudErrorClass::Unknown => {
            format!("{} failed with an unclassified cloud error.", err.provider)
        }
    };

    ClassifiedCloudError {
        class,
        action,
        friendly_message,
    }
}

/// True if `text` contains any of `needles` (all lowercase).
fn any(text: &str, needles: &[&str]) -> bool {
    needles.iter().any(|n| text.contains(n))
}

/// Whether an HTTP status code appears as a standalone token in `text`.
/// Guards against matching e.g. "529" inside a longer number.
fn has_code(text: &str, code: &str) -> bool {
    let bytes = text.as_bytes();
    let mut from = 0;
    while let Some(rel) = text[from..].find(code) {
        let start = from + rel;
        let end = start + code.len();
        let before_ok = start == 0 || !bytes[start - 1].is_ascii_digit();
        let after_ok = end >= bytes.len() || !bytes[end].is_ascii_digit();
        if before_ok && after_ok {
            return true;
        }
        from = start + 1;
    }
    false
}

/// Classify a cloud provider's CLI failure into a [`CloudErrorClass`].
///
/// * `provider` — backend name (`claude`, `codex`, `kimi`, `gemini`, `grok`);
///   case-insensitive. Drives the few vendor-specific quirks.
/// * `exit_code` — the CLI's process exit code, if known.
/// * `output` — combined stdout + stderr of the failed invocation.
///
/// Order is deliberate: quota is checked before rate-limit (both can be 429),
/// overload before generic 5xx, auth before everything 4xx.
pub fn classify(provider: &str, _exit_code: Option<i32>, output: &str) -> CloudErrorClass {
    let p = provider.to_ascii_lowercase();
    let t = output.to_ascii_lowercase();

    // 1. Auth — token expired / missing / invalid.
    if has_code(&t, "401")
        || any(
            &t,
            &[
                "authentication_error",
                "invalid_api_key",
                "invalid x-api-key",
                "unauthorized",
                "not authenticated",
                "please run",
                "login first",
                "no api key",
                "expired token",
                "oauth token has expired",
            ],
        )
    {
        return CloudErrorClass::Unauthenticated;
    }

    // 2. Quota / billing exhausted — check BEFORE rate-limit (both can be 429).
    if has_code(&t, "402")
        || any(
            &t,
            &[
                "insufficient_quota",
                "exceeded_current_quota",
                "exceeded your current quota",
                "payment_required",
                "billing",
                "out of credits",
                "credit balance",
                "quota exhausted",
                "exceeded_current_quota_error",
                "requests per day",
                "rpd",
                "usage limit",
            ],
        )
    {
        return CloudErrorClass::QuotaExhausted;
    }

    // 3. Overload — vendor-specific codes converge here.
    //    claude 529; openai/gemini 503; kimi 429 overload subtype.
    let overload_words = any(
        &t,
        &[
            "overloaded",
            "overload",
            "service unavailable",
            "currently unavailable",
            "high demand",
            "temporarily unable",
            "engine is currently overloaded",
        ],
    );
    if has_code(&t, "529")
        || overload_words
        || (has_code(&t, "503") && !any(&t, &["bad gateway"]))
        || (p == "kimi" && has_code(&t, "429") && overload_words)
    {
        return CloudErrorClass::Overloaded;
    }

    // 4. Rate limit (429 without quota markers).
    if has_code(&t, "429")
        || any(
            &t,
            &[
                "rate_limit",
                "rate limit",
                "too many requests",
                "rate_limit_reached",
                "rate_limit_exceeded",
                "resource_exhausted",
                "requests per minute",
                "tokens per minute",
                "slow down",
            ],
        )
    {
        return CloudErrorClass::RateLimited;
    }

    // 5. Context too long.
    if has_code(&t, "413")
        || any(
            &t,
            &[
                "context_length_exceeded",
                "request_too_large",
                "maximum context length",
                "too many tokens",
                "reduce the length",
                "prompt is too long",
                "input is too long",
            ],
        )
    {
        return CloudErrorClass::ContextTooLong;
    }

    // 6. Timeout / deadline.
    if has_code(&t, "504")
        || any(
            &t,
            &[
                "deadline_exceeded",
                "deadline expired",
                "timed out",
                "timeout",
                "request timed out",
            ],
        )
    {
        return CloudErrorClass::Timeout;
    }

    // 7. Model not found.
    if has_code(&t, "404") && any(&t, &["model", "not found", "does not exist", "deprecated"])
        || any(&t, &["model_not_found", "no such model"])
    {
        return CloudErrorClass::ModelNotFound;
    }

    // 8. Forbidden / geo / precondition.
    if has_code(&t, "403")
        || any(
            &t,
            &[
                "permission_error",
                "permission denied",
                "failed_precondition",
                "not supported for the api use",
                "unsupported_country",
                "country, region",
                "access denied",
            ],
        )
    {
        return CloudErrorClass::Forbidden;
    }

    // 9. Content filter.
    if any(
        &t,
        &[
            "content_filter",
            "content policy",
            "content management policy",
            "flagged",
            "safety",
            "blocked by",
        ],
    ) {
        return CloudErrorClass::ContentFiltered;
    }

    // 10. Generic upstream 5xx (not overload).
    if has_code(&t, "500")
        || has_code(&t, "502")
        || any(&t, &["internal server error", "api_error", "bad gateway"])
    {
        return CloudErrorClass::Transient5xx;
    }

    // 11. Network-level.
    if any(
        &t,
        &[
            "connection refused",
            "connection reset",
            "could not connect",
            "dns",
            "name or service not known",
            "network is unreachable",
            "tls",
            "broken pipe",
            "econnrefused",
        ],
    ) {
        return CloudErrorClass::Network;
    }

    // 12. Bad request.
    if has_code(&t, "400")
        || has_code(&t, "422")
        || any(&t, &["invalid_request", "invalid_argument", "bad request"])
    {
        return CloudErrorClass::BadRequest;
    }

    CloudErrorClass::Unknown
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cloud_error_class_is_transient_matrix() {
        for class in [
            CloudErrorClass::Overloaded,
            CloudErrorClass::RateLimited,
            CloudErrorClass::Transient5xx,
            CloudErrorClass::Timeout,
            CloudErrorClass::Network,
        ] {
            assert!(class.is_transient());
        }

        for class in [
            CloudErrorClass::QuotaExhausted,
            CloudErrorClass::Unauthenticated,
            CloudErrorClass::BadRequest,
            CloudErrorClass::Forbidden,
            CloudErrorClass::ContextTooLong,
        ] {
            assert!(!class.is_transient());
        }
    }

    #[test]
    fn claude_529_is_overloaded() {
        // The exact string from the Claude CLI banner.
        let c = classify(
            "claude",
            Some(1),
            "API Error: 529 Overloaded. This is a server-side issue, usually temporary — try again in a moment.",
        );
        assert_eq!(c, CloudErrorClass::Overloaded);
        assert!(c.is_transient());
        assert_eq!(c.action(), ErrorAction::RetryBackoff { max_attempts: 3 });
    }

    #[test]
    fn openai_503_is_overloaded_not_5xx() {
        let c = classify(
            "codex",
            Some(1),
            "Error 503: The engine is currently overloaded, please try again later",
        );
        assert_eq!(c, CloudErrorClass::Overloaded);
    }

    #[test]
    fn gemini_503_unavailable_is_overloaded() {
        let c = classify(
            "gemini",
            None,
            "503 UNAVAILABLE: The model is overloaded. Please try again later.",
        );
        assert_eq!(c, CloudErrorClass::Overloaded);
    }

    #[test]
    fn openai_429_quota_is_quota_not_ratelimit() {
        let c = classify(
            "codex",
            Some(1),
            "Error code: 429 - insufficient_quota: You exceeded your current quota",
        );
        assert_eq!(c, CloudErrorClass::QuotaExhausted);
        assert_eq!(c.action(), ErrorAction::SwitchProviderAlert);
        assert!(!c.is_transient());
    }

    #[test]
    fn openai_429_ratelimit_is_ratelimit() {
        let c = classify(
            "codex",
            Some(1),
            "Error code: 429 - rate_limit_exceeded: Rate limit reached for requests",
        );
        assert_eq!(c, CloudErrorClass::RateLimited);
    }

    #[test]
    fn kimi_quota_is_quota() {
        let c = classify(
            "kimi",
            Some(1),
            "429 exceeded_current_quota_error: your account balance is insufficient",
        );
        assert_eq!(c, CloudErrorClass::QuotaExhausted);
    }

    #[test]
    fn claude_429_rate_limit() {
        let c = classify(
            "claude",
            Some(1),
            "rate_limit_error: Your account has hit a rate limit (429)",
        );
        assert_eq!(c, CloudErrorClass::RateLimited);
    }

    #[test]
    fn anthropic_401_is_auth() {
        let c = classify(
            "claude",
            Some(1),
            "authentication_error (401): invalid x-api-key",
        );
        assert_eq!(c, CloudErrorClass::Unauthenticated);
        assert_eq!(c.action(), ErrorAction::FlipAuthThenSwitch);
    }

    #[test]
    fn grok_402_is_quota() {
        let c = classify(
            "grok",
            Some(1),
            "402 payment_required: promotional credits ran out",
        );
        assert_eq!(c, CloudErrorClass::QuotaExhausted);
    }

    #[test]
    fn gemini_504_is_timeout() {
        let c = classify(
            "gemini",
            None,
            "504 DEADLINE_EXCEEDED: Deadline expired before operation could complete",
        );
        assert_eq!(c, CloudErrorClass::Timeout);
    }

    #[test]
    fn gemini_400_failed_precondition_is_forbidden() {
        let c = classify(
            "gemini",
            None,
            "400 FAILED_PRECONDITION: User location is not supported for the API use",
        );
        assert_eq!(c, CloudErrorClass::Forbidden);
    }

    #[test]
    fn claude_413_is_context_too_long() {
        let c = classify(
            "claude",
            Some(1),
            "request_too_large (413): prompt is too long: 250000 tokens > 200000 maximum",
        );
        assert_eq!(c, CloudErrorClass::ContextTooLong);
        assert_eq!(c.action(), ErrorAction::CompactThenContinue);
    }

    #[test]
    fn context_length_exceeded_openai() {
        let c = classify(
            "codex",
            Some(1),
            "context_length_exceeded: maximum context length is 128000 tokens",
        );
        assert_eq!(c, CloudErrorClass::ContextTooLong);
    }

    #[test]
    fn model_not_found() {
        let c = classify(
            "grok",
            Some(1),
            "404 model not found: grok-2 is deprecated, migrate to grok-4.3",
        );
        assert_eq!(c, CloudErrorClass::ModelNotFound);
    }

    #[test]
    fn bad_request_is_terminal() {
        let c = classify(
            "codex",
            Some(1),
            "400 invalid_request: missing required parameter 'model'",
        );
        assert_eq!(c, CloudErrorClass::BadRequest);
        assert_eq!(c.action(), ErrorAction::Terminal);
    }

    #[test]
    fn network_refused_is_network() {
        let c = classify(
            "kimi",
            Some(1),
            "error sending request: connection refused (os error 61)",
        );
        assert_eq!(c, CloudErrorClass::Network);
    }

    #[test]
    fn internal_500_is_transient5xx() {
        let c = classify(
            "claude",
            Some(1),
            "500 api_error: An unexpected error has occurred internal to Anthropic's systems",
        );
        assert_eq!(c, CloudErrorClass::Transient5xx);
    }

    #[test]
    fn unrecognized_is_unknown() {
        let c = classify("codex", Some(1), "the cat sat on the mat");
        assert_eq!(c, CloudErrorClass::Unknown);
        // conservative single retry
        assert_eq!(c.action(), ErrorAction::RetryBackoff { max_attempts: 1 });
    }

    #[test]
    fn has_code_rejects_embedded_digits() {
        assert!(has_code("status 429 reached", "429"));
        assert!(!has_code("id 14299 created", "429"));
        assert!(has_code("(429)", "429"));
    }
}
