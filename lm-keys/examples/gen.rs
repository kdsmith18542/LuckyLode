//! Dev helper: print a testnet keypair (fixed secret + fresh random) for
//! node cross-checks (`validateaddress`, `importprivkey` on regtest).

use lm_keys::address::Keypair;
use lm_keys::chain::Network;
use lm_keys::hd::receive_key;
use lm_keys::mnemonic::SeedPhrase;

fn main() {
    let k = Keypair::from_secret([7u8; 32]).expect("key");
    println!("fixed secret 0707..07");
    println!("  wif:     {}", k.wif(Network::Testnet));
    println!("  p2pkh:   {}", k.p2pkh_address(Network::Testnet));
    println!("  regtest: {}", k.p2pkh_address(Network::Regtest));

    let phrase = SeedPhrase::generate().expect("phrase");
    let seed = phrase.seed();
    let k0 = receive_key(&seed, Network::Testnet, 0, 0).expect("receive key");
    println!("fresh phrase: {}", phrase.phrase());
    println!("  m/44'/1'/0'/0/0 p2pkh: {}", k0.p2pkh_address(Network::Testnet));
}
