/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! Cryptographically random, process-local namespace for opaque authority tokens.

use std::fmt;

use rand::TryRng;
use rand::rngs::SysRng;

pub const TOKEN_NAMESPACE_BYTES: usize = 16;
pub const TOKEN_NAMESPACE_HEX_BYTES: usize = TOKEN_NAMESPACE_BYTES * 2;

/// A 128-bit namespace generated from the operating system CSPRNG.
///
/// The namespace is serialized only as part of an opaque authority token. Diagnostics and debug
/// output must never expose it.
#[derive(Clone, Eq, Hash, PartialEq)]
pub struct OpaqueTokenNamespace([u8; TOKEN_NAMESPACE_BYTES]);

impl OpaqueTokenNamespace {
    /// Obtain a fresh namespace from the operating system CSPRNG.
    pub fn generate() -> Result<Self, TokenNamespaceError> {
        let mut bytes = [0; TOKEN_NAMESPACE_BYTES];
        SysRng
            .try_fill_bytes(&mut bytes)
            .map_err(|_| TokenNamespaceError)?;
        Ok(Self(bytes))
    }

    /// Construct a namespace from owner-supplied bytes.
    ///
    /// This is an internal seam for deterministic same-build tests. Production callers must use
    /// [`Self::generate`].
    #[doc(hidden)]
    pub const fn new_internal(bytes: [u8; TOKEN_NAMESPACE_BYTES]) -> Self {
        Self(bytes)
    }

    pub(crate) fn append_hex(&self, output: &mut String) {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        for byte in self.0 {
            output.push(HEX[usize::from(byte >> 4)] as char);
            output.push(HEX[usize::from(byte & 0x0f)] as char);
        }
    }
}

impl fmt::Debug for OpaqueTokenNamespace {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("OpaqueTokenNamespace(<redacted>)")
    }
}

/// Secret-free failure to obtain authority-token entropy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TokenNamespaceError;

pub(crate) fn format_namespaced_token(
    prefix: &str,
    namespace: &OpaqueTokenNamespace,
    alias: impl fmt::Display,
) -> String {
    let mut token = String::with_capacity(prefix.len() + TOKEN_NAMESPACE_HEX_BYTES + 1 + 39);
    token.push_str(prefix);
    namespace.append_hex(&mut token);
    token.push(':');
    use fmt::Write;
    write!(&mut token, "{alias}").expect("writing to a String cannot fail");
    token
}

/// Split a canonical namespaced token into its lowercase hexadecimal namespace and decimal alias.
pub(crate) fn split_namespaced_token<'a>(
    token: &'a str,
    prefix: &str,
) -> Result<(&'a str, &'a str), &'static str> {
    let Some(remainder) = token.strip_prefix(prefix) else {
        return Err("opaque token prefix required");
    };
    let Some((namespace, alias)) = remainder.split_once(':') else {
        return Err("canonical namespaced opaque token required");
    };
    if namespace.len() != TOKEN_NAMESPACE_HEX_BYTES
        || !namespace
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err("canonical lowercase hexadecimal token namespace required");
    }
    if alias.is_empty()
        || alias == "0"
        || (alias.len() > 1 && alias.starts_with('0'))
        || !alias.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err("canonical nonzero opaque token alias required");
    }
    Ok((namespace, alias))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn namespaced_tokens_are_canonical_and_namespace_debug_is_redacted() {
        let namespace = OpaqueTokenNamespace::new_internal([0xab; TOKEN_NAMESPACE_BYTES]);
        let token = format_namespaced_token("document:", &namespace, 7);
        assert_eq!(token, "document:abababababababababababababababab:7");
        assert_eq!(
            split_namespaced_token(&token, "document:"),
            Ok(("abababababababababababababababab", "7"))
        );
        assert_eq!(format!("{namespace:?}"), "OpaqueTokenNamespace(<redacted>)");
    }

    #[test]
    fn namespaced_token_parser_rejects_noncanonical_namespaces_and_aliases() {
        for token in [
            "document:abababababababababababababababa:1",
            "document:ababababababababababababababababa:1",
            "document:ABABABABABABABABABABABABABABABAB:1",
            "document:gggggggggggggggggggggggggggggggg:1",
            "document:abababababababababababababababab:0",
            "document:abababababababababababababababab:01",
            "document:abababababababababababababababab:+1",
            "document:abababababababababababababababab:1:2",
        ] {
            assert!(
                split_namespaced_token(token, "document:").is_err(),
                "accepted noncanonical token {token:?}",
            );
        }
    }
}
