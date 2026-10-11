//! Known-answer tests for EmberHash v1 — frozen vectors from doc 05 §5.4.
//! Any failure here is a consensus-breaking bug: fix the code, never the vectors.

use lm_pow::pow_full;

fn header_v() -> Vec<u8> {
    (0u8..80).collect()
}

fn hex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

#[test]
fn kat_v1_nonce0_height1() {
    let f = pow_full(&header_v(), 0, 1);
    assert_eq!(
        f.seed[..],
        hex("c0d0cb7a3009c163ef6d92fa2626158077777553b794c8fa6f3d29a7c7f26e86")[..]
    );
    assert_eq!(
        f.mem[..],
        hex("446765c0245c7d3da7185456343055971444c45dd90b015d4fd7b3c6262a2b99")[..]
    );
    assert_eq!(
        f.out[..],
        hex("ff0d88d9bf49d0d5c47a78b9a8f5478640359c4da7e73ee23973d6c338e2ca6e")[..]
    );
}

#[test]
fn kat_v2_nonce1_height1() {
    let f = pow_full(&header_v(), 1, 1);
    assert_eq!(
        f.seed[..],
        hex("88c16e6cb829970c72730f29ccad9448b425727340f492a47885357f95c6d549")[..]
    );
    assert_eq!(
        f.mem[..],
        hex("3924a6a0bfefe126c881fc7764794d6f45b992f5f22d33b2fd339af5bf405dbe")[..]
    );
    assert_eq!(
        f.out[..],
        hex("9c50add868c939b69096279e08387ae4593524d095cdd83a4ef5ea09908c67fd")[..]
    );
}

#[test]
fn kat_v3_max_nonce_height12345() {
    let f = pow_full(&header_v(), u64::MAX, 12345);
    assert_eq!(
        f.seed[..],
        hex("ee822cda19fa6523becfdaa72ac7a8581ac9316b340f9ca7648c7c8b45777cef")[..]
    );
    assert_eq!(
        f.mem[..],
        hex("48063374526971416c8c7d8f7b4e4ab815d4e4c6a4956024d2c3b459817a2791")[..]
    );
    assert_eq!(
        f.out[..],
        hex("335c53228b27bfd9b867774886df4226246727913d6ed8a7933c019b93cf24a0")[..]
    );
}
