use rand::Rng;

/// Generate a random id like `<prefix>-x7k2pq`: the given prefix plus six
/// chars from an unambiguous alphabet (no 0/o/1/l/i lookalikes).
pub(crate) fn random_id(prefix: &str) -> String {
    let mut rng = rand::rng();
    const ID_CHARSET: &[u8] = b"abcdefghjkmnpqrstuvwxyz23456789";
    let short_id: String = (0..6)
        .map(|_| {
            let idx = rng.random_range(0..ID_CHARSET.len());
            ID_CHARSET[idx] as char
        })
        .collect();
    format!("{prefix}-{short_id}")
}
