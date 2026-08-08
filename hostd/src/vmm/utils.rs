//! Small shared helpers for the VMM layer.

use rand::seq::IndexedRandom;

/// Generate a random id like `<prefix>-x7k2pq`: the given prefix plus six
/// chars from an unambiguous alphabet (no 0/o/1/l/i lookalikes).
pub(crate) fn random_id(prefix: &str) -> String {
    const ID_CHARSET: &[u8] = b"abcdefghjkmnpqrstuvwxyz23456789";
    let mut rng = rand::rng();
    let short_id: String = (0..6)
        .map(|_| {
            *ID_CHARSET
                .choose(&mut rng)
                .expect("ID_CHARSET is non-empty") as char
        })
        .collect();
    format!("{prefix}-{short_id}")
}
