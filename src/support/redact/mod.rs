//! Secret masking for free text, applied to everything the server stores.
//!
//! [`redact_text`] masks, in order:
//!
//! 1. **Credential formats** anywhere in the text (`sk-`, `sk-ant-`, `ghp_`/`gho_`/`ghs_`/`ghu_`,
//!    `github_pat_`, `xox?-`, `AKIA…`, `AIza…`, `glpat-`, `hf_`, `npm_`, JWTs, `Bearer `/`Basic `
//!    credentials, PEM blocks).
//! 2. **Assignments** `NAME=value` / `NAME: value` whose name looks secret (`api_key`, `token`,
//!    `secret`, `password`, `passwd`, `credential`): the value only.
//! 3. **URL secrets**: userinfo becomes `••••` (`https://••••@host`) and secret-looking query
//!    values are masked.
//! 4. **Entropy**: whitespace-separated words that look random (see `rules::is_high_entropy`).
//!    UUIDs, paths, URLs and word-like identifiers are exempt.

mod rules;

use rules::{MASK, is_high_entropy, mask_tokens};

/// Masks secrets in free text (note titles, bodies, tags): credential formats, `NAME=value`
/// assignments with secret-looking names, URL userinfo and secret query values, and
/// whitespace-separated words that pass the entropy heuristic.
pub fn redact_text(text: &str) -> String {
    let masked = mask_tokens(text);

    if !masked.split_whitespace().any(is_high_entropy) {
        return masked;
    }

    let mut out = String::with_capacity(masked.len());
    let mut rest = masked.as_str();

    // Rebuild word by word so the original whitespace (including newlines) is preserved.
    while let Some(start) = rest.find(|character: char| !character.is_whitespace()) {
        out.push_str(&rest[..start]);
        rest = &rest[start..];

        let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        let word = &rest[..end];
        out.push_str(if is_high_entropy(word) { MASK } else { word });
        rest = &rest[end..];
    }

    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn credential_formats_are_masked_anywhere() {
        let samples = [
            "sk-proj-abcdefghijklmnop1234",
            "sk-ant-api03-abcdefghijklmnopqrstu",
            "ghp_abcdefghijklmnopqrstuvwxyz0123",
            "gho_abcdefghijklmnopqrstuvwxyz0123",
            "ghs_abcdefghijklmnopqrstuvwxyz0123",
            "ghu_abcdefghijklmnopqrstuvwxyz0123",
            "github_pat_11ABCDEFG0123456789_abcdef",
            "xoxb-1234567890-abcdefghij",
            "AKIAIOSFODNN7EXAMPLE",
            "AIzaSyA-abcdefghijklmnopqrstuvwxyz01234",
            "glpat-abcdefghijklmnopqrst",
            "hf_abcdefghijklmnopqrstuvwxyz",
            "npm_abcdefghijklmnopqrstuvwxyz0123",
            "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0In0.signaturepart",
            "Bearer abcdefgh12345678",
            "Basic dXNlcjpwYXNzd29yZA==",
            "-----BEGIN OPENSSH PRIVATE KEY-----\nabc\n-----END OPENSSH PRIVATE KEY-----",
            "prefix text with sk-ant-abcdefghijklmnopqrstuvwx inside",
        ];

        for sample in samples {
            assert!(redact_text(sample).contains(MASK), "{sample}");
        }
    }

    #[test]
    fn url_userinfo_and_secret_query_values_are_masked_but_the_url_stays_readable() {
        assert_eq!(
            redact_text("https://user:pa55@proxy.local:3128/v1"),
            "https://••••@proxy.local:3128/v1"
        );
        assert_eq!(
            redact_text("see https://mcp.example.com/sse?sig=abc123&page=2 now"),
            "see https://mcp.example.com/sse?sig=••••&page=2 now"
        );
    }

    #[test]
    fn high_entropy_words_are_masked() {
        for sample in [
            "aB3dE5fG7hJ9kL1mN3pQ5rS7tU9vW1xY3zA5bC7d",
            "Xk9#mP2$vL8@nQ4!wR6^tY0&uI3*oE5(",
        ] {
            assert_eq!(redact_text(sample), MASK, "{sample}");
        }
    }

    #[test]
    fn ordinary_long_values_stay_visible() {
        let visible = [
            "This is a perfectly normal sentence about configuration.",
            "/Users/Frogius/Projekty/harness-useful-desktop2/src",
            "C:\\Users\\Frogius\\AppData\\Roaming\\npm\\node_modules",
            "https://github.com/MSpiechowicz/harness-useful-desktop",
            // UUIDs are identifiers (session ids), not credentials: shown.
            "7218123d-0746-47cd-b886-473b29bc5f49",
            "anthropic/claude-3-5-sonnet-20241022-v2:0",
            "us.anthropic.claude-sonnet-4-5-20250929-v1:0",
            "openrouter/meta-llama/llama-3.1-405b-instruct",
        ];

        for sample in visible {
            assert_eq!(redact_text(sample), sample, "{sample}");
        }
    }

    #[test]
    fn redact_text_masks_tokens_assignments_userinfo_and_random_words() {
        let input = "token sk-ant-abcdefghijklmnopqrstuv used\n\
                     GITHUB_TOKEN=abc123 password: hunter2\n\
                     clone https://me:pw@host/x.git id 7218123d-0746-47cd-b886-473b29bc5f49\n\
                     blob aB3dE5fG7hJ9kL1mN3pQ5rS7tU9vW1xY3zA5bC7d end";

        let expected = "token •••• used\n\
                        GITHUB_TOKEN=•••• password: ••••\n\
                        clone https://••••@host/x.git id 7218123d-0746-47cd-b886-473b29bc5f49\n\
                        blob •••• end";

        assert_eq!(redact_text(input), expected);
    }

    #[test]
    fn redact_text_leaves_plain_text_untouched() {
        let input = "Compiling harness_memory v0.1.0\n  Finished in 1.2s";

        assert_eq!(redact_text(input), input);
    }
}
