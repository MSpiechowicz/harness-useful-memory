//! Data tables and pattern checks behind [`super::redact_text`].

use std::sync::LazyLock;

use regex::Regex;

/// Replacement shown in place of hidden text.
pub(super) const MASK: &str = "••••";

/// Key-name fragments (normalized: lowercase, `_`/`-` removed) that mark a value as secret.
pub(super) const SECRET_KEY_FRAGMENTS: &[&str] = &[
    "apikey",
    "token",
    "secret",
    "password",
    "passwd",
    "credential",
    "authorization",
    "bearer",
    "privatekey",
    "clientsecret",
    "accesskey",
    "sessionkey",
    "cookie",
    "signature",
    "credentialid",
];

/// Query parameter names (normalized) whose values are masked inside URLs, besides key-name rules.
const SECRET_QUERY_PARAMS: &[&str] = &["key", "sig", "code", "auth", "pass", "pwd"];

/// Lowercases and drops `_`/`-` so `api_key`, `API-Key` and `apiKey` compare equal.
pub(super) fn normalize_key(key: &str) -> String {
    key.chars()
        .filter(|character| *character != '_' && *character != '-')
        .flat_map(char::to_lowercase)
        .collect()
}

/// True when the (raw) key name contains a secret fragment.
pub(super) fn is_secret_key(key: &str) -> bool {
    let key = normalize_key(key);
    SECRET_KEY_FRAGMENTS.iter().any(|fragment| key.contains(fragment))
}

// The patterns below are string literals checked by the test suite, so `Regex::new` cannot fail
// at runtime; `expect` documents that invariant.
fn compile(pattern: &str) -> Regex {
    Regex::new(pattern).expect("static regex is valid")
}

/// Well-known credential formats, matched anywhere in a string.
static TOKEN: LazyLock<Regex> = LazyLock::new(|| {
    compile(concat!(
        r"\bsk-(?:ant-)?[A-Za-z0-9_-]{16,}",
        r"|\bgh[opsu]_[A-Za-z0-9]{20,}",
        r"|\bgithub_pat_[A-Za-z0-9_]{20,}",
        r"|\bxox[abprs]-[A-Za-z0-9-]{10,}",
        r"|\bAKIA[0-9A-Z]{16}\b",
        r"|\bAIza[0-9A-Za-z_-]{35}",
        r"|\bglpat-[A-Za-z0-9_-]{20,}",
        r"|\bhf_[A-Za-z0-9]{20,}",
        r"|\bnpm_[A-Za-z0-9]{20,}",
        r"|\beyJ[A-Za-z0-9_-]+\.eyJ[A-Za-z0-9_-]+\.[A-Za-z0-9_-]*",
        r"|\bBearer\s+[A-Za-z0-9._~+/=-]{8,}",
        r"|\bBasic\s+[A-Za-z0-9+/]{8,}={0,2}",
        r"|-----BEGIN [A-Z0-9 ]+-----(?s:.*?)(?:-----END [A-Z0-9 ]+-----|\z)",
    ))
});

/// `NAME=value` / `NAME: value` assignments in free text where NAME looks secret.
static ASSIGNMENT: LazyLock<Regex> = LazyLock::new(|| {
    compile(concat!(
        r"(?i)\b([A-Za-z0-9_.-]*(?:api[_-]?key|token|secret|password|passwd|credential)",
        r#"[A-Za-z0-9_.-]*\s*[=:]\s*["']?)([^\s"',;]+)"#,
    ))
});

/// A URL with userinfo: group 1 = scheme, group 2 = userinfo.
static URL_USERINFO: LazyLock<Regex> = LazyLock::new(|| compile(r"\b([A-Za-z][A-Za-z0-9+.-]*://)([^/\s?#@]+)@"));

/// `name=value` pairs inside a URL query: group 1 = separator + name + `=`, group 2 = name, 3 = value.
static QUERY_PARAM: LazyLock<Regex> = LazyLock::new(|| compile(r"([?&]([A-Za-z0-9_.-]+)=)([^&#\s]*)"));

static URL: LazyLock<Regex> = LazyLock::new(|| compile(r"^[A-Za-z][A-Za-z0-9+.-]*://\S+$"));

static UUID: LazyLock<Regex> =
    LazyLock::new(|| compile(r"^[0-9A-Fa-f]{8}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{12}$"));

/// Word-like identifier segments: `claude`, `20241022`, `Sonnet`, `v2`, `405b`, `128k`, `llama3`.
static WORD_SEGMENT: LazyLock<Regex> =
    LazyLock::new(|| compile(r"^(?:[a-z]+|[0-9]+|[A-Z][a-z]+|[A-Za-z0-9]{1,3}|[0-9]+[a-z]{1,2}|[a-z]+[0-9]+)$"));

/// True when `value` is a single URL (`scheme://…` without whitespace).
pub(super) fn is_url(value: &str) -> bool {
    URL.is_match(value)
}

/// Masks userinfo and secret-looking query values in every URL inside `text`.
pub(super) fn mask_urls(text: &str) -> String {
    let masked = URL_USERINFO.replace_all(text, format!("${{1}}{MASK}@"));

    QUERY_PARAM
        .replace_all(&masked, |captures: &regex::Captures<'_>| {
            let name = &captures[2];
            let secret = is_secret_key(name) || SECRET_QUERY_PARAMS.contains(&normalize_key(name).as_str());

            if secret {
                format!("{}{MASK}", &captures[1])
            } else {
                captures[0].to_owned()
            }
        })
        .into_owned()
}

/// A URL anywhere in free text, up to whitespace, a quote or a bracket.
static URL_IN_TEXT: LazyLock<Regex> = LazyLock::new(|| compile(r#"\b[A-Za-z][A-Za-z0-9+.-]*://[^\s"'<>()\[\]{}`]+"#));

/// True when `value` is a shell or template reference (`$NAME`, `${NAME}`, `"$NAME"`), not a
/// literal.
fn is_reference(value: &str) -> bool {
    value.trim_start_matches(['"', '\'']).starts_with('$')
}

/// True when `text` holds a credential format (as [`TOKEN`] matches it). A PEM block only counts
/// when it is a private key, so a script may look for `-----BEGIN CERTIFICATE-----`.
pub(super) fn has_token(text: &str) -> bool {
    TOKEN.find_iter(text).any(|found| {
        let found = found.as_str();
        !found.starts_with("-----BEGIN ") || found.contains("PRIVATE KEY")
    })
}

/// True when a URL in `text` carries a literal password (`scheme://user:password@`) or a literal
/// value in a secret-looking query parameter. Values that reference a variable (`$TOKEN`) and a
/// user without a password (`ssh://git@host`) do not count.
pub(super) fn has_url_credentials(text: &str) -> bool {
    URL_IN_TEXT.find_iter(text).any(|url| {
        let url = url.as_str();

        let password = URL_USERINFO
            .captures(url)
            .and_then(|captures| captures.get(2))
            .and_then(|userinfo| userinfo.as_str().split_once(':'))
            .is_some_and(|(_, password)| !password.is_empty() && !is_reference(password));

        if password {
            return true;
        }

        QUERY_PARAM.captures_iter(url).any(|captures| {
            let name = &captures[2];
            let value = &captures[3];
            let secret = is_secret_key(name) || SECRET_QUERY_PARAMS.contains(&normalize_key(name).as_str());

            secret && !value.is_empty() && !is_reference(value)
        })
    })
}

/// Masks credential formats, secret assignments and URL secrets in free text.
pub(super) fn mask_tokens(text: &str) -> String {
    let text = TOKEN.replace_all(text, MASK);
    let text = ASSIGNMENT.replace_all(&text, format!("${{1}}{MASK}"));
    mask_urls(&text)
}

/// Minimum length for the entropy heuristic.
const ENTROPY_MIN_LEN: usize = 32;

/// Minimum Shannon entropy (bits per character) for the entropy heuristic.
const ENTROPY_MIN_BITS: f64 = 4.0;

/// Heuristic for unlabelled random secrets: no whitespace, ≥ 32 chars, ≥ 3 character classes
/// (lower, upper, digit, symbol) and Shannon entropy ≥ 4.0 bits/char.
///
/// Exempt because they are common, non-secret and otherwise trip the heuristic: paths, URLs
/// (their secrets are handled by [`mask_urls`]), UUIDs (identifiers, not credentials) and
/// identifiers made only of word-like segments such as `anthropic/claude-3-5-sonnet-20241022-v2:0`.
/// False positives (an unusual identifier shown as a secret) are accepted over false negatives.
pub(super) fn is_high_entropy(value: &str) -> bool {
    let long_enough = value.chars().count() >= ENTROPY_MIN_LEN;
    if !long_enough || value.chars().any(char::is_whitespace) {
        return false;
    }

    if is_path_like(value) || is_url(value) || UUID.is_match(value) || is_segmented_identifier(value) {
        return false;
    }

    character_classes(value) >= 3 && shannon_entropy(value) >= ENTROPY_MIN_BITS
}

fn is_path_like(value: &str) -> bool {
    let bytes = value.as_bytes();
    let windows_drive = bytes.len() > 2
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'\\' || bytes[2] == b'/');

    windows_drive
        || ["/", "~/", "./", "../", "\\\\"]
            .iter()
            .any(|prefix| value.starts_with(prefix))
}

/// True when every segment between `- _ . / : @` is word-like (see `WORD_SEGMENT`): a lowercase
/// word, a number, a capitalized word, at most three characters, a size like `405b`, or a
/// versioned word like `llama3`.
fn is_segmented_identifier(value: &str) -> bool {
    value
        .split(['-', '_', '.', '/', ':', '@'])
        .filter(|segment| !segment.is_empty())
        .all(|segment| WORD_SEGMENT.is_match(segment))
}

fn character_classes(value: &str) -> usize {
    let lower = value.chars().any(|character| character.is_ascii_lowercase());
    let upper = value.chars().any(|character| character.is_ascii_uppercase());
    let digit = value.chars().any(|character| character.is_ascii_digit());
    let symbol = value.chars().any(|character| !character.is_ascii_alphanumeric());

    [lower, upper, digit, symbol]
        .into_iter()
        .filter(|present| *present)
        .count()
}

fn shannon_entropy(value: &str) -> f64 {
    let mut counts = std::collections::HashMap::<char, usize>::new();
    for character in value.chars() {
        *counts.entry(character).or_default() += 1;
    }

    let total = value.chars().count() as f64;
    counts
        .values()
        .map(|count| {
            let probability = *count as f64 / total;
            -probability * probability.log2()
        })
        .sum()
}
