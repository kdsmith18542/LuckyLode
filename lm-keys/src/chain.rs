//! Network parameters, pinned to the node's `chainparams.cpp`.
//!
//! Address/key version bytes are consensus-adjacent (a wrong prefix makes
//! an address another network's): every value here cites its source line.

/// Which chain a key/address belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Network {
    Mainnet,
    Testnet,
    Regtest,
}

impl Network {
    /// P2PKH version byte: mainnet 48 (`R…`), testnet 111 (`m…`/`n…`).
    /// chainparams.cpp:170 (mainnet), :463 (testnet), :652 (regtest).
    pub fn p2pkh_version(self) -> u8 {
        match self {
            Network::Mainnet | Network::Regtest => 48,
            Network::Testnet => 111,
        }
    }

    /// P2SH version byte: mainnet 50, testnet 196 (`2…`).
    /// chainparams.cpp:171, :464, :653.
    pub fn p2sh_version(self) -> u8 {
        match self {
            Network::Mainnet | Network::Regtest => 50,
            Network::Testnet => 196,
        }
    }

    /// WIF version byte (always compressed keys): mainnet 188,
    /// testnet 128. chainparams.cpp:175, :468, :657.
    pub fn wif_version(self) -> u8 {
        match self {
            Network::Mainnet | Network::Regtest => 188,
            Network::Testnet => 128,
        }
    }

    /// BIP44 coin type for `m/44'/coin'/…`: 1 (testnet convention) on
    /// testnet and regtest. Mainnet is undecided until the genesis
    /// ceremony (needs a registered SLIP-44 id); 121 (Verus lineage) is
    /// the placeholder.
    pub fn coin_type(self) -> u32 {
        match self {
            Network::Mainnet => 121,
            Network::Testnet | Network::Regtest => 1,
        }
    }

    /// Extended private-key version bytes (node-compatible):
    /// xprv `0x0488ADE4` on mainnet, tprv `0x04358394` elsewhere.
    /// chainparams.cpp:177, :470.
    pub fn xprv_version(self) -> [u8; 4] {
        match self {
            Network::Mainnet => [0x04, 0x88, 0xAD, 0xE4],
            Network::Testnet | Network::Regtest => [0x04, 0x35, 0x83, 0x94],
        }
    }

    /// Extended public-key version bytes: xpub `0x0488B21E` on mainnet,
    /// tpub `0x043587CF` elsewhere.
    pub fn xpub_version(self) -> [u8; 4] {
        match self {
            Network::Mainnet => [0x04, 0x88, 0xB2, 0x1E],
            Network::Testnet | Network::Regtest => [0x04, 0x35, 0x87, 0xCF],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn regtest_shares_mainnet_prefixes() {
        assert_eq!(Network::Regtest.p2pkh_version(), 48);
        assert_eq!(Network::Regtest.wif_version(), 188);
    }

    #[test]
    fn testnet_prefixes_are_bitcoin_testnet() {
        assert_eq!(Network::Testnet.p2pkh_version(), 111);
        assert_eq!(Network::Testnet.p2sh_version(), 196);
        assert_eq!(Network::Testnet.wif_version(), 128);
    }
}
