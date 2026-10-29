//! Secret masking for free text, applied to everything the server stores.
//!
//! [`redact_text`] masks, in order:
//!
//! 1. **Credential formats** anywhere in the text (`sk-`, `sk-ant-`, `ghp_`/`gho_`/`ghs_`/`ghu_`,
//!    `github_pat_`, `xox?-`, `AKIA…`, `AIza…`, `glpat-`, `hf_`, `npm_`, `sk_live_`/`sk_test_`/`rk_live_`, JWTs, `Bearer `/`Basic `
//!    credentials, PEM blocks).
//! 2. **Assignments** `NAME=value` / `NAME: value` whose name looks secret (`api_key`, `token`,
//!    `secret`, `password`, `passwd`, `credential`): the value only.
//! 3. **URL secrets**: userinfo becomes `••••` (`https://••••@host`) and secret-looking query
//!    values are masked.
//! 4. **Entropy**: whitespace-separated words that look random (see `rules::is_high_entropy`).
//!    UUIDs, paths, URLs and word-like identifiers are exempt.
//!
//! [`has_credentials`] only asks whether text holds a credential format or a URL with a literal
//! password or secret query value, for script bodies that are stored as written.

mod rules;

use rules::{MASK, has_token, has_url_credentials, is_high_entropy, mask_formats, mask_tokens};

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

/// Masks script output: credential formats, secret assignments and URL secrets, as
/// [`redact_text`] does, but not the entropy rule, which would mask compact JSON words.
pub fn redact_output(text: &str) -> String {
    mask_tokens(text)
}

/// Masks credential formats and URL secrets only, for text that is code: a name like `token: String` or
/// `API_KEY = x` is how code is written, and masking the value after it would hide the type, not a secret.
pub fn redact_formats(text: &str) -> String {
    mask_formats(text)
}

/// True when `text` holds a credential format (as [`redact_text`] masks them; a PEM block only
/// when it is a private key) or a URL with a literal password or secret query value. Variable
/// references (`$GH_TOKEN`, `https://u:${PASS}@host`), `NAME=value` assignments and random-looking
/// words do not count: a script reads its secrets from the environment.
pub fn has_credentials(text: &str) -> bool {
    has_token(text) || has_url_credentials(text)
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn code_keeps_its_assignments_while_credential_formats_and_url_secrets_are_masked() {
        let token = format!("gh{}_{}", "p", "A1b2C3d4E5f6G7h8I9j0K1l2M3n4");

        assert_eq!(
            redact_formats("pub token: String, password: &str"),
            "pub token: String, password: &str"
        );
        assert_eq!(redact_formats(&format!("fn {token}()")), "fn ••••()");
        assert_eq!(
            redact_formats("let u = https://user:pa55@host/p?sig=abc"),
            "let u = https://••••@host/p?sig=••••"
        );
        // the assignment rule of the free-text scrubber is what this one leaves out
        assert_eq!(redact_output("API_KEY = hunter2"), "API_KEY = ••••");
        assert_eq!(redact_formats("API_KEY = hunter2"), "API_KEY = hunter2");
    }

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
    fn stripe_style_keys_are_masked_in_text_and_in_code() {
        let tail = "4eC39HqLyjWDarjtT1zdp7dc";

        for prefix in ["sk_live_", "sk_test_", "rk_live_"] {
            let key = [prefix, tail].concat();

            assert!(has_credentials(&key), "{prefix}");
            assert_eq!(redact_formats(&format!("let k = {key};")), "let k = ••••;", "{prefix}");
        }

        // too short to be a key, and a name that only starts like one
        assert!(!has_credentials(&["sk_live_", "short"].concat()));
        assert!(!has_credentials("risk_live_values_of_the_market"));
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
            concat!("aB3dE5fG7hJ9kL1mN3pQ5", "rS7tU9vW1xY3zA5bC7d"),
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
        let input = concat!(
            "token sk-ant-abcdefghijklmnopqrstuv used\n\
                     GITHUB_TOKEN=abc123 password: hunter2\n\
                     clone https://me:pw@host/x.git id 7218123d-0746-47cd-b886-473b29bc5f49\n\
                     blob aB3dE5fG7hJ9kL1mN3pQ5",
            "rS7tU9vW1xY3zA5bC7d end"
        );

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

    #[test]
    fn credentials_in_script_bodies_are_found() {
        let found = [
            format!(
                "curl -H 'Authorization: token {}' https://api.github.com",
                "ghp_".to_owned() + &"a".repeat(36)
            ),
            concat!("export KEY=AKIA", "ABCDEFGHIJKLMNOP").to_owned(),
            "curl -H \"Authorization: Bearer abcdefgh12345678\" $URL".to_owned(),
            "git clone https://me:pa55word@example.com/x.git".to_owned(),
            "fetch('https://api.example.com/v1?api_key=abc123&page=2')".to_owned(),
            "curl \"https://hooks.example.com/run?sig=deadbeef\"".to_owned(),
            "cat <<KEY\n-----BEGIN OPENSSH PRIVATE KEY-----\nabc\n-----END OPENSSH PRIVATE KEY-----\nKEY".to_owned(),
        ];

        for text in found {
            assert!(has_credentials(&text), "{text}");
        }
    }

    #[test]
    fn variable_references_and_plain_scripts_are_not_credentials() {
        let clean = [
            "#!/usr/bin/env bash\nset -euo pipefail\ngh pr list --json number,title",
            "curl -H \"Authorization: Bearer $GH_TOKEN\" https://api.github.com/user",
            "git clone https://x-access-token:${GH_TOKEN}@github.com/o/r.git",
            "git clone https://user:$PASSWORD@example.com/x.git",
            "git clone ssh://git@github.com/o/r.git",
            "curl \"https://api.example.com/v1?api_key=$API_KEY&page=2\"",
            "GH_TOKEN=$(gh auth token) gh api /user",
            "password = os.environ['DB_PASSWORD']",
            concat!("echo aB3dE5fG7hJ9kL1mN3pQ5", "rS7tU9vW1xY3zA5bC7d"),
            "openssl x509 -in cert.pem | grep -q -- '-----BEGIN CERTIFICATE-----'",
        ];

        for text in clean {
            assert!(!has_credentials(text), "{text}");
        }
    }

    #[test]
    fn script_output_keeps_compact_json_and_masks_credentials() {
        let json = r#"[{"number":18,"title":"feat(extensions): update plugins"},{"number":6,"title":"docs:"}]"#;
        assert_eq!(redact_output(json), json);
        assert_ne!(
            redact_text(json),
            json,
            "the entropy rule is what redact_output leaves out"
        );

        let token = format!("ghp_{}", "a1B2c3D4e5".repeat(4).get(..36).expect("36"));
        assert!(!redact_output(&format!("token {token}")).contains(&token));
    }
}
