//! Private-repository credential for git dependencies (shared §5).
//!
//! Git's own credential helper and ssh agent keep working because kong runs the
//! git CLI. This module only adds the optional token taken from the environment.

use tracing::debug;

use crate::node::git::fetch::GitEnv;

/// Builds the git child environment from a token read through `get`.
///
/// `KONG_GIT_TOKEN` applies to every https host (`http.extraheader`); otherwise
/// `GITHUB_TOKEN`, then `GH_TOKEN`, apply to `https://github.com/` only. A value
/// that is empty after trimming counts as unset.
///
/// The token is passed as a `GIT_CONFIG_*` extraheader in the git child's
/// environment only: it never reaches argv (visible in `ps`), a URL, `kong.rules`
/// or a store path, so it cannot be written anywhere (quality bar 7). `secrets`
/// lists the token and the header so `fetch::redact` masks both in any git
/// stderr kong prints. Prompt suppression is `fetch::run_git`'s job.
///
/// Tests pass a closure here instead of setting process environment variables.
pub fn git_env_from(get: &dyn Fn(&str) -> Option<String>) -> GitEnv {
    const SOURCES: [(&str, &str); 3] = [
        ("KONG_GIT_TOKEN", "http.extraheader"),
        ("GITHUB_TOKEN", "http.https://github.com/.extraheader"),
        ("GH_TOKEN", "http.https://github.com/.extraheader"),
    ];
    for (source, key) in SOURCES {
        let Some(value) = get(source) else { continue };
        let token = value.trim();
        if token.is_empty() {
            continue;
        }
        // Only the variable name is logged, never its value.
        debug!(source = source, "git token supplied by the environment");
        let header = basic_auth_header(token);
        return GitEnv {
            vars: vec![
                ("GIT_CONFIG_COUNT".to_string(), "1".to_string()),
                ("GIT_CONFIG_KEY_0".to_string(), key.to_string()),
                ("GIT_CONFIG_VALUE_0".to_string(), header.clone()),
            ],
            secrets: vec![token.to_string(), header],
        };
    }
    GitEnv::default()
}

/// The credential environment of this machine's process environment.
pub fn machine_git_env() -> GitEnv {
    git_env_from(&|key| std::env::var(key).ok())
}

/// The HTTP header GitHub (and most git hosts) accept for a token over https.
///
/// `x-access-token` is the user name GitHub expects for app and personal tokens.
pub fn basic_auth_header(token: &str) -> String {
    format!(
        "AUTHORIZATION: basic {}",
        base64(format!("x-access-token:{token}").as_bytes())
    )
}

/// Standard-alphabet base64 with `=` padding (RFC 4648), local to avoid a crate.
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[((n >> (18 - 6 * i)) & 0x3f) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::git::fetch::redact;

    fn env_of(pairs: &[(&str, &str)]) -> GitEnv {
        let pairs: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        git_env_from(&|key| pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone()))
    }

    fn var<'a>(env: &'a GitEnv, name: &str) -> Option<&'a str> {
        env.vars
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    #[test]
    fn base64_matches_rfc4648_vectors() {
        let vectors = [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ];
        for (input, expected) in vectors {
            assert_eq!(base64(input.as_bytes()), expected, "base64({input:?})");
        }
    }

    #[test]
    fn header_encodes_x_access_token() {
        assert_eq!(
            basic_auth_header("abc"),
            format!("AUTHORIZATION: basic {}", base64(b"x-access-token:abc"))
        );
        assert_eq!(
            basic_auth_header("abc"),
            "AUTHORIZATION: basic eC1hY2Nlc3MtdG9rZW46YWJj"
        );
    }

    #[test]
    fn kong_token_wins_and_applies_to_every_host() {
        let env = env_of(&[
            ("KONG_GIT_TOKEN", "kong"),
            ("GITHUB_TOKEN", "gh"),
            ("GH_TOKEN", "cli"),
        ]);
        assert_eq!(var(&env, "GIT_CONFIG_COUNT"), Some("1"));
        assert_eq!(var(&env, "GIT_CONFIG_KEY_0"), Some("http.extraheader"));
        assert_eq!(
            var(&env, "GIT_CONFIG_VALUE_0"),
            Some(basic_auth_header("kong").as_str())
        );
        assert_eq!(env.vars.len(), 3);
    }

    #[test]
    fn github_token_wins_over_gh_token_for_github_only() {
        let env = env_of(&[("GITHUB_TOKEN", "gh"), ("GH_TOKEN", "cli")]);
        assert_eq!(
            var(&env, "GIT_CONFIG_KEY_0"),
            Some("http.https://github.com/.extraheader")
        );
        assert_eq!(
            var(&env, "GIT_CONFIG_VALUE_0"),
            Some(basic_auth_header("gh").as_str())
        );

        let env = env_of(&[("GH_TOKEN", "cli")]);
        assert_eq!(
            var(&env, "GIT_CONFIG_KEY_0"),
            Some("http.https://github.com/.extraheader")
        );
        assert_eq!(
            var(&env, "GIT_CONFIG_VALUE_0"),
            Some(basic_auth_header("cli").as_str())
        );
    }

    #[test]
    fn empty_or_blank_token_is_ignored() {
        let env = env_of(&[
            ("KONG_GIT_TOKEN", ""),
            ("GITHUB_TOKEN", " \t\n"),
            ("GH_TOKEN", "cli"),
        ]);
        assert_eq!(
            var(&env, "GIT_CONFIG_KEY_0"),
            Some("http.https://github.com/.extraheader")
        );
        assert_eq!(env.secrets[0], "cli");

        let env = env_of(&[("KONG_GIT_TOKEN", "  "), ("GH_TOKEN", "")]);
        assert!(env.vars.is_empty());
        assert!(env.secrets.is_empty());
    }

    #[test]
    fn no_token_gives_default_env() {
        let env = env_of(&[]);
        assert!(env.vars.is_empty());
        assert!(env.secrets.is_empty());
    }

    #[test]
    fn secrets_hold_token_and_header_and_redact_masks_them() {
        let env = env_of(&[("KONG_GIT_TOKEN", "  s3cr3t-sentinel\n")]);
        let header = basic_auth_header("s3cr3t-sentinel");
        assert_eq!(
            env.secrets,
            vec!["s3cr3t-sentinel".to_string(), header.clone()]
        );

        let stderr = format!("fatal: config value '{header}' rejected");
        let redacted = redact(&stderr, &env);
        assert_eq!(redacted, "fatal: config value '***' rejected");
        assert!(!redacted.contains("s3cr3t-sentinel"));
        assert_eq!(
            redact("token s3cr3t-sentinel leaked", &env),
            "token *** leaked"
        );
    }
}
