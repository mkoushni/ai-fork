// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Praxis Contributors

//! A shared helper for body-derived data promotion.

use praxis_filter::{
    FilterError,
    builtins::http::{
        payload_processing::config_validation::validate_header_name, value_safety::is_safe_promoted_value,
    },
};

// -----------------------------------------------------------------------------
// Constants
// -----------------------------------------------------------------------------

/// Longest value that may be promoted to a header, metadata key, or filter result.
pub const MAX_PROMOTED_VALUE_LEN: usize = 256;

// -----------------------------------------------------------------------------
// is_promotable_value
// -----------------------------------------------------------------------------

/// Returns `true` iff `val` is within the length limit and safe for HTTP header use.
pub fn is_promotable_value(val: &str) -> bool {
    val.len() <= MAX_PROMOTED_VALUE_LEN && is_safe_promoted_value(val)
}

/// Hop-by-hop, framing, Host, and proxy-auth names that must not be
/// used as promotion-header targets.
///
/// Composes [`crate::http_hop::is_hop_by_hop`] with `Host` and
/// `Content-Length`, which are transport-controlled but not hop-by-hop.
pub fn is_transport_controlled_header(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    name == "content-length" || name == "host" || crate::http_hop::is_hop_by_hop(&name)
}

/// Whether `name` is unsafe as a body-derived promotion-header target.
///
/// Blocks transport-controlled names, request credentials, and internal
/// `x-praxis-*` headers outside the AI fact namespaces (`x-praxis-ai-*`,
/// `x-praxis-responses-*`).
pub fn is_unsafe_promotion_header(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    is_transport_controlled_header(&name) || is_credential_header(&name) || is_unrelated_internal_header(&name)
}

/// Validate syntax and reject unsafe promotion-header targets.
///
/// # Errors
///
/// Returns [`FilterError`] when the name is empty, not a valid HTTP
/// header name, or an unsafe promotion target.
pub fn validate_promotion_header(filter: &str, field: &str, name: Option<&str>) -> Result<(), FilterError> {
    validate_header_name(filter, field, name)?;
    let Some(raw) = name else {
        return Ok(());
    };
    reject_unsafe_promotion_target(filter, field, raw)
}

/// Credential names that must not receive a client-derived model string.
fn is_credential_header(name: &str) -> bool {
    matches!(name, "authorization" | "cookie" | "set-cookie" | "www-authenticate")
}

/// Internal Praxis headers that are not AI classification/rewrite facts.
fn is_unrelated_internal_header(name: &str) -> bool {
    name.starts_with("x-praxis-") && !name.starts_with("x-praxis-ai-") && !name.starts_with("x-praxis-responses-")
}

/// Reject a parsed promotion target that would overwrite transport or routing state.
fn reject_unsafe_promotion_target(filter: &str, field: &str, raw: &str) -> Result<(), FilterError> {
    let Ok(parsed) = http::HeaderName::from_bytes(raw.as_bytes()) else {
        return Ok(());
    };
    if is_unsafe_promotion_header(parsed.as_str()) {
        return Err(format!(
            "{filter}: '{field}' must not use transport, credential, or internal header '{}'",
            parsed.as_str()
        )
        .into());
    }
    Ok(())
}

#[cfg(test)]
#[expect(clippy::allow_attributes, reason = "blanket test suppressions")]
#[allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]
mod tests {
    use super::*;

    #[test]
    fn accepts_normal_model() {
        assert!(
            is_promotable_value("gpt-4.1"),
            "short ASCII model name should be promotable"
        );
    }

    #[test]
    fn rejects_oversized_value() {
        let long = "x".repeat(257);
        assert!(!is_promotable_value(&long), "257-byte value should be rejected");
    }

    #[test]
    fn accepts_value_at_limit() {
        let exact = "x".repeat(256);
        assert!(is_promotable_value(&exact), "256-byte value should be accepted");
    }

    #[test]
    fn rejects_newline() {
        assert!(!is_promotable_value("bad\nmodel"), "newline should be rejected");
    }

    #[test]
    fn accepts_empty_string() {
        assert!(is_promotable_value(""), "empty string should be accepted");
    }

    #[test]
    fn transport_controlled_headers_are_blocked() {
        for name in [
            "content-length",
            "Content-Length",
            "host",
            "transfer-encoding",
            "proxy-authorization",
            "connection",
        ] {
            assert!(
                is_transport_controlled_header(name),
                "transport header '{name}' should be blocked"
            );
        }
    }

    #[test]
    fn promotion_defaults_are_not_transport_controlled() {
        assert!(
            !is_transport_controlled_header("x-praxis-ai-effective-model"),
            "default promotion header must remain allowed"
        );
    }

    #[test]
    fn unsafe_promotion_blocks_authorization_and_unrelated_internal() {
        for name in ["authorization", "Authorization", "cookie", "x-praxis-route"] {
            assert!(
                is_unsafe_promotion_header(name),
                "promotion header '{name}' should be blocked"
            );
        }
    }

    #[test]
    fn unsafe_promotion_allows_ai_fact_namespaces() {
        for name in [
            "x-praxis-ai-effective-model",
            "x-praxis-ai-model",
            "x-praxis-responses-mode",
            "x-custom-model",
        ] {
            assert!(
                !is_unsafe_promotion_header(name),
                "promotion header '{name}' should remain allowed"
            );
        }
    }

    #[test]
    fn validate_promotion_header_rejects_authorization() {
        let err = validate_promotion_header("test", "model", Some("authorization")).unwrap_err();
        assert!(
            err.to_string().contains("transport, credential, or internal header"),
            "authorization should be rejected: {err}"
        );
    }
}
