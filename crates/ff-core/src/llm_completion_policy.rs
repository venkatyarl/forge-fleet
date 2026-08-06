//! Provider-neutral safety policy for OpenAI-compatible chat completions.
//!
//! The policy deliberately operates on [`serde_json::Value`] so routing crates
//! can share one set of request and response rules without depending on a
//! particular provider's wire types.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Historical completion budget used by ForgeFleet callers that did not set one.
pub const LEGACY_DEFAULT_COMPLETION_TOKENS: u32 = 2_048;

/// Absolute completion-token ceiling accepted by the shared policy.
pub const HARD_MAX_COMPLETION_TOKENS: u32 = 32_768;

/// The workload semantics that affect generation policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WorkloadClass {
    /// Deliberative work where a provider may use its normal reasoning mode.
    #[serde(rename = "reasoning")]
    Reasoning,
    /// A bounded, public-answer task such as a one-shot code generation call.
    #[serde(rename = "code_oneshot", alias = "code_one_shot")]
    CodeOneShot,
}

/// A validated, positive completion-token budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct CompletionBudget(u32);

impl CompletionBudget {
    /// Validate a completion-token budget against the shared hard ceiling.
    pub fn new(tokens: u32) -> Result<Self, CompletionBudgetError> {
        if tokens == 0 {
            return Err(CompletionBudgetError::Zero);
        }
        if tokens > HARD_MAX_COMPLETION_TOKENS {
            return Err(CompletionBudgetError::ExceedsHardCap {
                requested: tokens,
                hard_cap: HARD_MAX_COMPLETION_TOKENS,
            });
        }
        Ok(Self(tokens))
    }

    /// Return the validated token count.
    pub const fn get(self) -> u32 {
        self.0
    }
}

impl Default for CompletionBudget {
    fn default() -> Self {
        Self(LEGACY_DEFAULT_COMPLETION_TOKENS)
    }
}

impl TryFrom<u32> for CompletionBudget {
    type Error = CompletionBudgetError;

    fn try_from(tokens: u32) -> Result<Self, Self::Error> {
        Self::new(tokens)
    }
}

/// Why a raw token count cannot become a [`CompletionBudget`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CompletionBudgetError {
    #[error("completion token budget must be positive")]
    Zero,
    #[error("completion token budget {requested} exceeds hard cap {hard_cap}")]
    ExceedsHardCap { requested: u32, hard_cap: u32 },
}

/// Why a chat request cannot safely receive the shared policy.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RequestPolicyError {
    #[error("chat completion request must be a JSON object")]
    RequestNotObject,
    #[error("chat_template_kwargs must be a JSON object when present")]
    ChatTemplateKwargsNotObject,
}

/// Apply the shared policy to an OpenAI-compatible chat-completion request.
///
/// Every workload gets an explicit `max_tokens`. Code one-shot requests also
/// get `chat_template_kwargs.enable_thinking=false`, which prevents a bounded
/// generation from spending its entire public-output budget on private
/// reasoning. Any other template kwargs are retained. A malformed existing
/// `chat_template_kwargs` fails without partially modifying the request.
pub fn apply_completion_policy(
    request: &mut Value,
    workload: WorkloadClass,
    budget: CompletionBudget,
) -> Result<(), RequestPolicyError> {
    let request = request
        .as_object_mut()
        .ok_or(RequestPolicyError::RequestNotObject)?;

    if workload == WorkloadClass::CodeOneShot {
        if let Some(kwargs) = request.get("chat_template_kwargs") {
            if !kwargs.is_object() {
                return Err(RequestPolicyError::ChatTemplateKwargsNotObject);
            }
        }
    }

    request.insert("max_tokens".to_string(), Value::from(budget.get()));

    if workload == WorkloadClass::CodeOneShot {
        let kwargs = request
            .entry("chat_template_kwargs".to_string())
            .or_insert_with(|| Value::Object(Map::new()))
            .as_object_mut()
            .expect("chat_template_kwargs was validated as an object");
        kwargs.insert("enable_thinking".to_string(), Value::Bool(false));
    }

    Ok(())
}

/// A public completion accepted by the fail-closed response policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedCompletion {
    /// Public model output. Private `reasoning_content` is never copied here.
    pub content: String,
}

/// Why an OpenAI-compatible response cannot be used as a completed answer.
///
/// Variants intentionally contain only structural facts or public protocol
/// fields. The response body, `reasoning_content`, and `reasoning` are never
/// retained.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CompletionValidationError {
    #[error("completion response is missing choices")]
    MissingChoices,
    #[error("completion response has null choices")]
    NullChoices,
    #[error("completion response choices must be an array")]
    ChoicesNotArray,
    #[error("completion response contains no choices")]
    NoChoices,
    #[error("completion response contains {actual} choices; exactly one is required")]
    MultipleChoices { actual: usize },
    #[error("completion choice must be a JSON object")]
    ChoiceNotObject,
    #[error("completion choice is missing finish_reason")]
    MissingFinishReason,
    #[error("completion choice has null finish_reason")]
    NullFinishReason,
    #[error("completion choice finish_reason must be a string")]
    FinishReasonNotString,
    #[error("completion was truncated at its token limit")]
    Length,
    #[error("completion was blocked by a content filter")]
    ContentFilter,
    #[error("completion has unsupported finish_reason {reason:?}")]
    UnknownFinishReason { reason: String },
    #[error("completion choice is missing public content")]
    MissingContent,
    #[error("completion choice has null public content")]
    NullContent,
    #[error("completion choice public content must be a string")]
    ContentNotString,
    #[error("completion choice public content is empty or whitespace")]
    EmptyContent,
    #[error("completion choice has an unclosed private reasoning block")]
    UnclosedReasoningBlock,
    #[error("completion choice contains a private reasoning tag after public output began")]
    EmbeddedReasoningBlock,
}

/// Remove provider-private leading reasoning blocks from otherwise public text.
///
/// Some OpenAI-compatible servers embed reasoning directly in `content` using
/// `<think>...</think>` or `<thinking>...</thinking>`, even when thinking was
/// disabled in the request. Leading blocks are removed as provider metadata;
/// tag-shaped text after public output begins and unclosed blocks fail closed
/// rather than risking disclosure of private reasoning.
pub fn sanitize_public_content(content: &str) -> Result<String, CompletionValidationError> {
    let mut remainder = content;
    let mut removed_private_block = false;

    loop {
        let candidate = remainder.trim_start();
        let block = if starts_with_ascii_case_insensitive(candidate, "<think>") {
            Some(("<think>", "</think>"))
        } else if starts_with_ascii_case_insensitive(candidate, "<thinking>") {
            Some(("<thinking>", "</thinking>"))
        } else {
            None
        };

        if let Some((opening, closing)) = block {
            let private = &candidate[opening.len()..];
            let end = find_ascii_case_insensitive(private, closing)
                .ok_or(CompletionValidationError::UnclosedReasoningBlock)?;
            remainder = &private[end + closing.len()..];
            removed_private_block = true;
            continue;
        }

        // Some templates consume the opening token and leave a leading close.
        if starts_with_ascii_case_insensitive(candidate, "</think>") {
            remainder = &candidate["</think>".len()..];
            removed_private_block = true;
            continue;
        }
        if starts_with_ascii_case_insensitive(candidate, "</thinking>") {
            remainder = &candidate["</thinking>".len()..];
            removed_private_block = true;
            continue;
        }
        break;
    }

    let public = if removed_private_block {
        remainder.trim()
    } else {
        content
    };
    let lower_public = public.to_ascii_lowercase();
    if lower_public.contains("<think") || lower_public.contains("</think") {
        return Err(CompletionValidationError::EmbeddedReasoningBlock);
    }
    if public.trim().is_empty() {
        return Err(CompletionValidationError::EmptyContent);
    }
    Ok(public.to_string())
}

fn starts_with_ascii_case_insensitive(value: &str, prefix: &str) -> bool {
    value
        .get(..prefix.len())
        .is_some_and(|candidate| candidate.eq_ignore_ascii_case(prefix))
}

fn find_ascii_case_insensitive(value: &str, needle: &str) -> Option<usize> {
    let needle = needle.as_bytes();
    value
        .as_bytes()
        .windows(needle.len())
        .position(|candidate| candidate.eq_ignore_ascii_case(needle))
}

/// Validate one non-streaming OpenAI-compatible chat-completion response.
///
/// Success requires exactly one choice, `finish_reason == "stop"`, and a
/// non-whitespace public answer. `choice.message.content` is preferred. The
/// legacy public `choice.text` field is accepted only when `message.content`
/// is absent or null so the same guard can protect older `/v1/completions`
/// adapters. Provider-private `reasoning_content` and `reasoning` fields are
/// never used as an answer.
pub fn validate_completion_response(
    response: &Value,
) -> Result<ValidatedCompletion, CompletionValidationError> {
    let choices = match response.get("choices") {
        None => return Err(CompletionValidationError::MissingChoices),
        Some(Value::Null) => return Err(CompletionValidationError::NullChoices),
        Some(Value::Array(choices)) => choices,
        Some(_) => return Err(CompletionValidationError::ChoicesNotArray),
    };

    let choice = match choices.len() {
        0 => return Err(CompletionValidationError::NoChoices),
        1 => choices[0]
            .as_object()
            .ok_or(CompletionValidationError::ChoiceNotObject)?,
        actual => return Err(CompletionValidationError::MultipleChoices { actual }),
    };

    match choice.get("finish_reason") {
        None => return Err(CompletionValidationError::MissingFinishReason),
        Some(Value::Null) => return Err(CompletionValidationError::NullFinishReason),
        Some(Value::String(reason)) if reason == "stop" => {}
        Some(Value::String(reason)) if reason == "length" => {
            return Err(CompletionValidationError::Length);
        }
        Some(Value::String(reason)) if reason == "content_filter" => {
            return Err(CompletionValidationError::ContentFilter);
        }
        Some(Value::String(reason)) => {
            return Err(CompletionValidationError::UnknownFinishReason {
                reason: reason.clone(),
            });
        }
        Some(_) => return Err(CompletionValidationError::FinishReasonNotString),
    }

    let (primary, primary_was_null) = match choice.get("message") {
        None => (None, false),
        Some(Value::Null) => (None, true),
        Some(Value::Object(message)) => match message.get("content") {
            None => (None, false),
            Some(Value::Null) => (None, true),
            Some(Value::String(content)) => (Some(content), false),
            Some(_) => return Err(CompletionValidationError::ContentNotString),
        },
        Some(_) => return Err(CompletionValidationError::ContentNotString),
    };

    let public_content = match primary {
        Some(content) => content,
        None => match choice.get("text") {
            Some(Value::String(text)) => text,
            Some(Value::Null) => return Err(CompletionValidationError::NullContent),
            Some(_) => return Err(CompletionValidationError::ContentNotString),
            None if primary_was_null => {
                return Err(CompletionValidationError::NullContent);
            }
            None => return Err(CompletionValidationError::MissingContent),
        },
    };

    Ok(ValidatedCompletion {
        content: sanitize_public_content(public_content)?,
    })
}

/// Recursively remove provider-private reasoning fields in place.
///
/// Both `reasoning_content` and `reasoning` exist in OpenAI-compatible
/// provider responses in the fleet, so both exact keys are treated as private.
///
/// Returns the number of fields removed, which is useful for redaction audits.
pub fn redact_reasoning_content(value: &mut Value) -> usize {
    match value {
        Value::Object(object) => {
            let removed = usize::from(object.remove("reasoning_content").is_some())
                + usize::from(object.remove("reasoning").is_some());
            removed
                + object
                    .values_mut()
                    .map(redact_reasoning_content)
                    .sum::<usize>()
        }
        Value::Array(values) => values.iter_mut().map(redact_reasoning_content).sum(),
        _ => 0,
    }
}

/// Clone a payload and recursively redact private reasoning before logging it.
pub fn redacted_for_logging(value: &Value) -> Value {
    let mut redacted = value.clone();
    redact_reasoning_content(&mut redacted);
    sanitize_loggable_content_fields(&mut redacted);
    redacted
}

fn sanitize_loggable_content_fields(value: &mut Value) {
    match value {
        Value::Object(object) => {
            for (key, field) in object {
                if matches!(key.as_str(), "content" | "text")
                    && let Value::String(content) = field
                {
                    *content = sanitize_public_content(content).unwrap_or_else(|_| {
                        "[redacted private or unsafe completion content]".to_string()
                    });
                    continue;
                }
                sanitize_loggable_content_fields(field);
            }
        }
        Value::Array(values) => {
            for item in values {
                sanitize_loggable_content_fields(item);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn response(content: Value, finish_reason: Value) -> Value {
        json!({
            "choices": [{
                "finish_reason": finish_reason,
                "message": {"content": content}
            }]
        })
    }

    #[test]
    fn completion_budget_default_is_valid_legacy_value() {
        let budget = CompletionBudget::default();
        assert_eq!(budget.get(), LEGACY_DEFAULT_COMPLETION_TOKENS);
        assert!(budget.get() <= HARD_MAX_COMPLETION_TOKENS);
    }

    #[test]
    fn workload_class_has_stable_tool_facing_names() {
        assert_eq!(
            serde_json::to_value(WorkloadClass::CodeOneShot).unwrap(),
            json!("code_oneshot")
        );
        assert_eq!(
            serde_json::from_value::<WorkloadClass>(json!("code_one_shot")).unwrap(),
            WorkloadClass::CodeOneShot
        );
        assert_eq!(
            serde_json::to_value(WorkloadClass::Reasoning).unwrap(),
            json!("reasoning")
        );
    }

    #[test]
    fn completion_budget_requires_positive_bounded_value() {
        assert_eq!(CompletionBudget::new(1).unwrap().get(), 1);
        assert_eq!(
            CompletionBudget::new(HARD_MAX_COMPLETION_TOKENS)
                .unwrap()
                .get(),
            HARD_MAX_COMPLETION_TOKENS
        );
        assert_eq!(CompletionBudget::new(0), Err(CompletionBudgetError::Zero));
        assert_eq!(
            CompletionBudget::new(HARD_MAX_COMPLETION_TOKENS + 1),
            Err(CompletionBudgetError::ExceedsHardCap {
                requested: HARD_MAX_COMPLETION_TOKENS + 1,
                hard_cap: HARD_MAX_COMPLETION_TOKENS,
            })
        );
    }

    #[test]
    fn reasoning_policy_sets_budget_without_changing_template_kwargs() {
        let mut request = json!({
            "model": "reasoning-model",
            "chat_template_kwargs": {"enable_thinking": true, "custom": "kept"}
        });
        apply_completion_policy(
            &mut request,
            WorkloadClass::Reasoning,
            CompletionBudget::new(4_096).unwrap(),
        )
        .unwrap();

        assert_eq!(request["max_tokens"], 4_096);
        assert_eq!(request["chat_template_kwargs"]["enable_thinking"], true);
        assert_eq!(request["chat_template_kwargs"]["custom"], "kept");
    }

    #[test]
    fn code_policy_creates_non_thinking_template_kwargs() {
        let mut request = json!({"model": "glm"});
        apply_completion_policy(
            &mut request,
            WorkloadClass::CodeOneShot,
            CompletionBudget::default(),
        )
        .unwrap();

        assert_eq!(request["max_tokens"], LEGACY_DEFAULT_COMPLETION_TOKENS);
        assert_eq!(request["chat_template_kwargs"]["enable_thinking"], false);
    }

    #[test]
    fn code_policy_preserves_other_kwargs_and_overwrites_thinking_flag() {
        let mut request = json!({
            "max_tokens": 99,
            "chat_template_kwargs": {"enable_thinking": true, "tools_in_user_message": false}
        });
        apply_completion_policy(
            &mut request,
            WorkloadClass::CodeOneShot,
            CompletionBudget::new(3_000).unwrap(),
        )
        .unwrap();

        assert_eq!(request["max_tokens"], 3_000);
        assert_eq!(request["chat_template_kwargs"]["enable_thinking"], false);
        assert_eq!(
            request["chat_template_kwargs"]["tools_in_user_message"],
            false
        );
    }

    #[test]
    fn malformed_code_request_fails_without_partial_mutation() {
        let mut request = json!({"max_tokens": 7, "chat_template_kwargs": "bad"});
        let before = request.clone();
        assert_eq!(
            apply_completion_policy(
                &mut request,
                WorkloadClass::CodeOneShot,
                CompletionBudget::default(),
            ),
            Err(RequestPolicyError::ChatTemplateKwargsNotObject)
        );
        assert_eq!(request, before);

        let mut scalar = json!(false);
        assert_eq!(
            apply_completion_policy(
                &mut scalar,
                WorkloadClass::Reasoning,
                CompletionBudget::default(),
            ),
            Err(RequestPolicyError::RequestNotObject)
        );
    }

    #[test]
    fn validates_exactly_one_stopped_nonempty_public_message() {
        let value = response(json!("  public answer  "), json!("stop"));
        assert_eq!(
            validate_completion_response(&value).unwrap(),
            ValidatedCompletion {
                content: "  public answer  ".to_string()
            }
        );
    }

    #[test]
    fn accepts_legacy_public_choice_text_when_message_content_is_absent() {
        let value = json!({"choices": [{"finish_reason": "stop", "text": "legacy answer"}]});
        assert_eq!(
            validate_completion_response(&value).unwrap().content,
            "legacy answer"
        );

        let null_primary = json!({
            "choices": [{
                "finish_reason": "stop",
                "message": {"content": null},
                "text": "legacy answer"
            }]
        });
        assert_eq!(
            validate_completion_response(&null_primary).unwrap().content,
            "legacy answer"
        );
    }

    #[test]
    fn choice_shape_errors_are_typed() {
        assert_eq!(
            validate_completion_response(&json!({})),
            Err(CompletionValidationError::MissingChoices)
        );
        assert_eq!(
            validate_completion_response(&json!({"choices": null})),
            Err(CompletionValidationError::NullChoices)
        );
        assert_eq!(
            validate_completion_response(&json!({"choices": {}})),
            Err(CompletionValidationError::ChoicesNotArray)
        );
        assert_eq!(
            validate_completion_response(&json!({"choices": []})),
            Err(CompletionValidationError::NoChoices)
        );
        assert_eq!(
            validate_completion_response(&json!({"choices": [{}, {}]})),
            Err(CompletionValidationError::MultipleChoices { actual: 2 })
        );
        assert_eq!(
            validate_completion_response(&json!({"choices": [null]})),
            Err(CompletionValidationError::ChoiceNotObject)
        );
    }

    #[test]
    fn finish_reason_errors_are_typed_and_fail_closed() {
        let choice = |finish_reason: Option<Value>| {
            let mut choice = json!({"message": {"content": "answer"}});
            if let Some(reason) = finish_reason {
                choice["finish_reason"] = reason;
            }
            json!({"choices": [choice]})
        };

        assert_eq!(
            validate_completion_response(&choice(None)),
            Err(CompletionValidationError::MissingFinishReason)
        );
        assert_eq!(
            validate_completion_response(&choice(Some(Value::Null))),
            Err(CompletionValidationError::NullFinishReason)
        );
        assert_eq!(
            validate_completion_response(&choice(Some(json!(12)))),
            Err(CompletionValidationError::FinishReasonNotString)
        );
        assert_eq!(
            validate_completion_response(&choice(Some(json!("length")))),
            Err(CompletionValidationError::Length)
        );
        assert_eq!(
            validate_completion_response(&choice(Some(json!("content_filter")))),
            Err(CompletionValidationError::ContentFilter)
        );
        assert_eq!(
            validate_completion_response(&choice(Some(json!("tool_calls")))),
            Err(CompletionValidationError::UnknownFinishReason {
                reason: "tool_calls".to_string()
            })
        );
    }

    #[test]
    fn public_content_errors_are_typed() {
        let stopped = |choice: Value| json!({"choices": [choice]});

        assert_eq!(
            validate_completion_response(&stopped(json!({"finish_reason": "stop"}))),
            Err(CompletionValidationError::MissingContent)
        );
        assert_eq!(
            validate_completion_response(&response(Value::Null, json!("stop"))),
            Err(CompletionValidationError::NullContent)
        );
        assert_eq!(
            validate_completion_response(&response(json!(false), json!("stop"))),
            Err(CompletionValidationError::ContentNotString)
        );
        assert_eq!(
            validate_completion_response(&stopped(json!({
                "finish_reason": "stop",
                "message": false,
                "text": "must not hide malformed chat content"
            }))),
            Err(CompletionValidationError::ContentNotString)
        );
        assert_eq!(
            validate_completion_response(&response(json!(" \n\t "), json!("stop"))),
            Err(CompletionValidationError::EmptyContent)
        );
    }

    #[test]
    fn glm_length_reasoning_only_incident_is_rejected_without_leaking_reasoning() {
        let secret_reasoning = "private chain from the 512-token GLM incident";
        let value = json!({
            "choices": [{
                "finish_reason": "length",
                "message": {
                    "content": null,
                    "reasoning_content": secret_reasoning
                }
            }],
            "usage": {"completion_tokens": 512}
        });

        let error = validate_completion_response(&value).unwrap_err();
        assert_eq!(error, CompletionValidationError::Length);
        assert!(!error.to_string().contains(secret_reasoning));
        assert!(!format!("{error:?}").contains(secret_reasoning));
    }

    #[test]
    fn reasoning_content_is_never_a_public_answer() {
        let secret_reasoning = "private reasoning only";
        let value = json!({
            "choices": [{
                "finish_reason": "stop",
                "message": {"reasoning_content": secret_reasoning}
            }]
        });
        let error = validate_completion_response(&value).unwrap_err();
        assert_eq!(error, CompletionValidationError::MissingContent);
        assert!(!format!("{error:?}").contains(secret_reasoning));
    }

    #[test]
    fn leading_private_reasoning_blocks_are_removed_from_public_content() {
        let value = response(
            json!("  <think>secret one</think>\n<thinking>secret two</thinking> answer "),
            json!("stop"),
        );
        assert_eq!(
            validate_completion_response(&value).unwrap().content,
            "answer"
        );

        let template_consumed_open = response(json!(" </THINK> visible answer"), json!("stop"));
        assert_eq!(
            validate_completion_response(&template_consumed_open)
                .unwrap()
                .content,
            "visible answer"
        );

        let uppercase = response(json!("<THINK>secret</THINK> public"), json!("stop"));
        assert_eq!(
            validate_completion_response(&uppercase).unwrap().content,
            "public"
        );

        let unicode_private = response(
            json!("<think>İstanbul reasoning</THINK> public"),
            json!("stop"),
        );
        assert_eq!(
            validate_completion_response(&unicode_private)
                .unwrap()
                .content,
            "public"
        );
    }

    #[test]
    fn unclosed_or_reasoning_only_public_content_fails_closed() {
        let unclosed = response(json!("<think>private and truncated"), json!("stop"));
        assert_eq!(
            validate_completion_response(&unclosed),
            Err(CompletionValidationError::UnclosedReasoningBlock)
        );

        let reasoning_only = response(json!("<think>private</think> \n"), json!("stop"));
        assert_eq!(
            validate_completion_response(&reasoning_only),
            Err(CompletionValidationError::EmptyContent)
        );

        let embedded = response(
            json!("public prefix <THINK>private</THINK> public suffix"),
            json!("stop"),
        );
        assert_eq!(
            validate_completion_response(&embedded),
            Err(CompletionValidationError::EmbeddedReasoningBlock)
        );
    }

    #[test]
    fn recursive_redaction_removes_private_reasoning_from_loggable_clone() {
        let original = json!({
            "reasoning_content": "top secret",
            "reasoning": "alternate top secret",
            "choices": [{
                "message": {
                    "content": "public",
                    "reasoning_content": "nested secret",
                    "nested": [{"reasoning_content": "deep secret", "kept": true}]
                }
            }]
        });

        let mut in_place = original.clone();
        assert_eq!(redact_reasoning_content(&mut in_place), 4);
        assert_eq!(in_place, redacted_for_logging(&original));
        let serialized = serde_json::to_string(&in_place).unwrap();
        assert!(!serialized.contains("reasoning_content"));
        assert!(!serialized.contains("reasoning"));
        assert!(!serialized.contains("secret"));
        assert_eq!(in_place["choices"][0]["message"]["content"], "public");
        assert_eq!(in_place["choices"][0]["message"]["nested"][0]["kept"], true);

        assert_eq!(original["reasoning_content"], "top secret");
    }

    #[test]
    fn loggable_clone_sanitizes_embedded_completion_reasoning() {
        let original = json!({
            "choices": [{
                "message": {"content": "<think>secret</think> public"},
                "text": "prefix <thinking>also secret</thinking> suffix"
            }]
        });
        let redacted = redacted_for_logging(&original);
        assert_eq!(redacted["choices"][0]["message"]["content"], "public");
        assert_eq!(
            redacted["choices"][0]["text"],
            "[redacted private or unsafe completion content]"
        );
        assert!(!redacted.to_string().contains("secret"));
    }
}
