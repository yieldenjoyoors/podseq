//! Engine API JWT auth; each request carries an HMAC-SHA256 bearer token whose `iat` must be within 60s of the server clock ([spec](https://github.com/ethereum/execution-apis/blob/main/src/engine/authentication.md)).

use alloy_rpc_types_engine::{Claims, JwtError, JwtSecret};

/// Engine API JWT secret used to mint per-request bearer tokens.
#[derive(Debug, Clone)]
pub struct Auth(JwtSecret);

impl Auth {
    /// Builds an [`Auth`] from a hex-encoded secret, tolerating an optional `0x` prefix.
    pub fn from_hex(hex: &str) -> Result<Self, JwtError> {
        Ok(Self(JwtSecret::from_hex(hex)?))
    }

    /// Builds an [`Auth`] by reading the hex secret from a file (e.g. Reth's `jwt.hex`).
    pub fn from_file(path: &std::path::Path) -> Result<Self, JwtError> {
        Ok(Self(JwtSecret::from_file(path)?))
    }

    /// Mints a signed JWT bearer token with the current `iat`, valid within the server's 60s window.
    pub fn token(&self) -> Result<String, jsonwebtoken::errors::Error> {
        let claims = Claims::with_current_timestamp();
        self.0.encode(&claims)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_SECRET: &str = "4d6ccb86b5f16e56e9d8859c64cf8b16c8a4d00ec3ee79416e0c4d56c57f9160";

    #[test]
    fn token_is_three_jwt_segments() {
        let auth = Auth::from_hex(TEST_SECRET).unwrap();
        let token = auth.token().unwrap();
        assert_eq!(token.split('.').count(), 3);
    }

    #[test]
    fn rejects_short_secret() {
        assert!(Auth::from_hex("abcd").is_err());
    }

    #[test]
    fn strips_optional_0x_prefix() {
        let with_prefix =
            Auth::from_hex("0x4d6ccb86b5f16e56e9d8859c64cf8b16c8a4d00ec3ee79416e0c4d56c57f9160");
        assert!(with_prefix.is_ok());
    }
}
