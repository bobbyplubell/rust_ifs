fn main() {
    let key = ed25519_dalek::SigningKey::from_bytes(&[0x5e_u8; 32]);
    let minter = key.verifying_key().to_bytes();
    let t = std::time::Instant::now();
    let g = sheep_proto::derive::derive_minted(1_700_000_000_000_000, &minter);
    println!("derive_minted(genesis) took {:?}; sheep_id={}",
        t.elapsed(), flame_core::canonical::sheep_id_hex(&g));
}
