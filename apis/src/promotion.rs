// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Praxis Contributors

//! A shared helper for body-derived data promotion.

use praxis_filter::builtins::http::value_safety::is_safe_promoted_value;

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
pub fn is_transport_controlled_header(name: &str) -> bool {
    [
        "connection",
        "content-length",
        "host",
        "keep-alive",
        "proxy-authenticate",
        "proxy-authorization",
        "proxy-connection",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
    ]
    .iter()
    .any(|blocked| name.eq_ignore_ascii_case(blocked))
}

#[cfg(test)]
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
}
