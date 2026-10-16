use sha2::{Digest, Sha256};

#[test]
fn embedded_datastar_script_is_the_audited_one() {
    let digest = hex::encode(Sha256::digest(super::DATASTAR_JS));
    assert_eq!(digest, super::DATASTAR_SHA256);
}
