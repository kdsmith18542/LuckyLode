//! BIP39 mnemonics (English wordlist): the wallet's seed backup format.

use bip39::{Language, Mnemonic, WordCount};

/// A validated 12-word seed phrase (BIP39 English, 128-bit entropy).
#[derive(Debug, Clone)]
pub struct SeedPhrase(Mnemonic);

impl SeedPhrase {
    /// Generate a fresh 12-word phrase (OS randomness).
    pub fn generate() -> Result<Self, String> {
        Mnemonic::generate_in(Language::English, WordCount::Words12)
            .map(SeedPhrase)
            .map_err(|e| format!("mnemonic generate: {e}"))
    }

    /// Parse + checksum-validate a user-supplied phrase.
    pub fn parse(s: &str) -> Result<Self, String> {
        Mnemonic::parse_in(Language::English, s)
            .map(SeedPhrase)
            .map_err(|e| format!("mnemonic: {e}"))
    }

    /// The phrase words (display / backup).
    pub fn phrase(&self) -> String {
        self.0.to_string()
    }

    /// 64-byte seed (PBKDF2-HMAC-SHA512, empty passphrase — the spending
    /// password, if ever added, goes here).
    pub fn seed(&self) -> [u8; 64] {
        self.0.to_seed_normalized("")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// BIP39 official test vector 1 (trezor, passphrase "TREZOR").
    #[test]
    fn official_vector_128bit() {
        let entropy = [0u8; 16];
        let m = Mnemonic::from_entropy(&entropy).expect("entropy");
        assert_eq!(
            m.to_string(),
            "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about"
        );
        assert_eq!(
            hex::encode(m.to_seed_normalized("TREZOR")),
            "c55257c360c07c72029aebc1b53c05ed0362ada38ead3e3e9efa3708e53495531f09a6987599d18264c1e1c92f2cf141630c7a3c4ab7c81b2f001698e7463b04"
        );
    }

    #[test]
    fn parse_roundtrip_and_rejects_bad_checksum() {
        let p = SeedPhrase::parse(
            "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about",
        )
        .expect("valid phrase");
        assert_eq!(p.phrase().split_whitespace().count(), 12);
        assert!(SeedPhrase::parse(
            "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon"
        )
        .is_err());
    }

    #[test]
    fn generate_gives_fresh_12_word_phrases() {
        let a = SeedPhrase::generate().expect("gen").phrase();
        let b = SeedPhrase::generate().expect("gen").phrase();
        assert_eq!(a.split_whitespace().count(), 12);
        assert_ne!(a, b);
        // And the fresh phrase parses back.
        assert!(SeedPhrase::parse(&a).is_ok());
    }
}
