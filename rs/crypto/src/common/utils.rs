//! Static crypto utility methods.
use ic_config::crypto::CryptoConfig;
use ic_crypto_internal_csp::api::CspSecretKeyStoreChecker;
use ic_crypto_internal_csp::secret_key_store::proto_store::ProtoSecretKeyStore;
use ic_crypto_internal_csp::types::{CspPop, CspPublicKey};
use ic_crypto_internal_csp::Csp;
use ic_crypto_internal_csp::{public_key_store, CryptoServiceProvider};
use ic_crypto_tls_interfaces::TlsPublicKeyCert;
use ic_crypto_utils_basic_sig::conversions as basicsig_conversions;
use ic_protobuf::crypto::v1::NodePublicKeys;
use ic_protobuf::registry::crypto::v1::PublicKey as PublicKeyProto;
use ic_protobuf::registry::crypto::v1::{AlgorithmId as AlgorithmIdProto, X509PublicKeyCert};
use ic_types::crypto::{AlgorithmId, CryptoError, CryptoResult};
use ic_types::NodeId;
use rand::rngs::OsRng;
use std::path::Path;
use std::sync::Arc;

pub mod ni_dkg;

mod temp_crypto;

use crate::keygen::{
    ensure_committee_signing_key_material_is_set_up_correctly,
    ensure_dkg_dealing_encryption_key_material_is_set_up_correctly,
    ensure_idkg_dealing_encryption_key_material_is_set_up_correctly,
    ensure_node_signing_key_material_is_set_up_correctly,
    ensure_tls_key_material_is_set_up_correctly,
};
use ic_crypto_internal_logmon::metrics::CryptoMetrics;
pub use temp_crypto::{NodeKeysToGenerate, TempCryptoComponent};
#[cfg(test)]
use tempfile::TempDir;

#[cfg(test)]
mod tests;

/// Generates (forward-secure) NI-DKG dealing encryption key material given the
/// `node_id` of the node.
///
/// The secret key is stored in the key store of the provided `csp`, while the corresponding
/// public key is returned by this function.
fn generate_dkg_dealing_encryption_keys(
    csp: &mut dyn CryptoServiceProvider,
    node_id: NodeId,
) -> PublicKeyProto {
    let (pubkey, pop) = csp
        .create_forward_secure_key_pair(AlgorithmId::NiDkg_Groth20_Bls12_381, node_id)
        .expect("Failed to generate DKG dealing encryption keys");
    ic_crypto_internal_csp::keygen::utils::dkg_dealing_encryption_pk_to_proto(pubkey, pop)
}

/// Generates (MEGa) I-DKG dealing encryption key material.
///
/// The secret key is stored in the key store of the provided `csp`, while the corresponding
/// public key is returned by this function.
fn generate_idkg_dealing_encryption_keys(csp: &mut dyn CryptoServiceProvider) -> PublicKeyProto {
    let pubkey = csp
        .idkg_create_mega_key_pair(AlgorithmId::ThresholdEcdsaSecp256k1)
        .expect("Failed to generate IDkg dealing encryption keys");

    PublicKeyProto {
        version: 0,
        algorithm: AlgorithmIdProto::MegaSecp256k1 as i32,
        key_value: pubkey.serialize(),
        proof_data: None,
    }
}

/// Obtains the node's cryptographic keys or generates them if they are missing.
///
/// To check/generate the keys, a CSP client is created according to the given `config`.
/// First, tries to retrieve the node's public keys from `config.crypto_root`. If they
/// exist and they are consistent with the secret keys in kept by the CSP, the
/// public keys are returned together with the corresponding node ID.
///
/// If they do not exist, new keys are generated: the secret parts are stored in
/// the secret key store of the CSP, and the public parts are stored in a
/// public key store at `config.crypto_root`. The keys are generated for a particular
/// node ID, which is derived from the node's signing public key. In particular,
/// the node's TLS certificate and the node's DKG dealing encryption key are
/// bound to this node ID. The newly generated public keys are then returned
/// together with the corresponding node ID.
///
/// The `config.crypto_root` directory must exist and have the [permissions required
/// for storing crypto state](CryptoConfig::check_dir_has_required_permissions).
/// If there exists no key store in `config.crypto_root`, a new one is created.
///
/// # Panics
///  * if public keys exist but are inconsistent with the secret keys.
///  * if an error occurs when accessing or generating the keys.
pub fn get_node_keys_or_generate_if_missing(
    config: &CryptoConfig,
    tokio_runtime_handle: Option<tokio::runtime::Handle>,
) -> (NodePublicKeys, NodeId) {
    let crypto_root = config.crypto_root.as_path();
    match check_keys_locally(config, tokio_runtime_handle.clone()) {
        Ok(None) => {
            // Generate new keys.
            let mut csp = csp_for_config(config, tokio_runtime_handle.clone());
            let committee_signing_pk = generate_committee_signing_keys(&csp);
            let node_signing_pk = generate_node_signing_keys(&csp);
            let node_id = derive_node_id(&node_signing_pk);
            let dkg_dealing_encryption_pk = generate_dkg_dealing_encryption_keys(&mut csp, node_id);
            let idkg_dealing_encryption_pk = generate_idkg_dealing_encryption_keys(&mut csp);
            let tls_certificate = generate_tls_keys(&mut csp, node_id).to_proto();
            let node_pks = NodePublicKeys {
                version: 1,
                node_signing_pk: Some(node_signing_pk),
                committee_signing_pk: Some(committee_signing_pk),
                tls_certificate: Some(tls_certificate),
                dkg_dealing_encryption_pk: Some(dkg_dealing_encryption_pk),
                idkg_dealing_encryption_pk: Some(idkg_dealing_encryption_pk),
            };
            public_key_store::store_node_public_keys(crypto_root, &node_pks)
                .unwrap_or_else(|_| panic!("Failed to store public key material"));
            // Re-check the generated keys.
            let stored_keys = check_keys_locally(config, tokio_runtime_handle)
                .expect("Could not read generated keys.")
                .expect("Newly generated keys are inconsistent.");
            if stored_keys != node_pks {
                panic!("Generated keys differ from the stored ones.");
            }
            (node_pks, node_id)
        }
        Ok(Some(mut node_pks)) => {
            // Generate I-DKG key if it is not present yet: we generate the key
            // purely based on whether it already exists and at the same time
            // set the key material version to 1, so that afterwards the
            // version will be consistent on all nodes, no matter what it was
            // before.
            if node_pks.idkg_dealing_encryption_pk.is_none() {
                let mut csp = csp_for_config(config, tokio_runtime_handle.clone());
                let idkg_dealing_encryption_pk = generate_idkg_dealing_encryption_keys(&mut csp);
                node_pks.idkg_dealing_encryption_pk = Some(idkg_dealing_encryption_pk);
                node_pks.version = 1;
                public_key_store::store_node_public_keys(crypto_root, &node_pks)
                    .unwrap_or_else(|_| panic!("Failed to store public key material"));
                // Re-check the generated keys.
                let stored_keys = check_keys_locally(config, tokio_runtime_handle)
                    .expect("Could not read generated keys.")
                    .expect("Newly generated keys are inconsistent.");
                if stored_keys != node_pks {
                    panic!("Generated keys differ from the stored ones.");
                }
            }
            let node_signing_pk = node_pks
                .node_signing_pk
                .as_ref()
                .expect("Missing node signing public key");
            let node_id = derive_node_id(node_signing_pk);
            (node_pks, node_id)
        }
        Err(e) => panic!("Node contains inconsistent key material: {}", e),
    }
}

pub fn derive_node_id(node_signing_pk: &PublicKeyProto) -> NodeId {
    basicsig_conversions::derive_node_id(node_signing_pk)
        .expect("Corrupted node signing public key")
}

fn generate_node_signing_keys(csp: &dyn CryptoServiceProvider) -> PublicKeyProto {
    let generated = csp
        .gen_key_pair(AlgorithmId::Ed25519)
        .expect("Could not generate node signing keys");
    match generated {
        CspPublicKey::Ed25519(pk) => PublicKeyProto {
            algorithm: AlgorithmIdProto::Ed25519 as i32,
            key_value: pk.0.to_vec(),
            version: 0,
            proof_data: None,
        },
        _ => panic!("Unexpected types"),
    }
}

fn read_public_keys(crypto_root: &Path) -> CryptoResult<NodePublicKeys> {
    public_key_store::read_node_public_keys(crypto_root).map_err(|e| CryptoError::InvalidArgument {
        message: format!("Failed reading public keys: {:?}", e),
    })
}

/// Checks whether this crypto component has complete local key material, i.e.
/// whether the public key store contains the required public keys, and whether
/// the secret key store contains the required secret keys.
/// Returns:
///  - `Ok(Some(node_public_keys))` if all public keys are found and they are
///    consistent with the secret keys.
///  - `Ok(None)` if no public keys are found.
///  - `Err(...)` in all other cases.
fn check_keys_locally(
    config: &CryptoConfig,
    tokio_runtime_handle: Option<tokio::runtime::Handle>,
) -> CryptoResult<Option<NodePublicKeys>> {
    let crypto_root = config.crypto_root.as_path();
    let node_pks = match read_public_keys(crypto_root) {
        Ok(pks) => pks,
        Err(_) => return Ok(None),
    };
    if node_public_keys_are_empty(&node_pks) {
        return Ok(None);
    }
    let csp = csp_for_config(config, tokio_runtime_handle);
    ensure_node_signing_key_is_set_up_locally(node_pks.node_signing_pk.clone(), &csp)?;
    ensure_committee_signing_key_is_set_up_locally(node_pks.committee_signing_pk.clone(), &csp)?;
    ensure_dkg_dealing_encryption_key_is_set_up_locally(
        node_pks.dkg_dealing_encryption_pk.clone(),
        &csp,
    )?;
    ensure_tls_cert_is_set_up_locally(node_pks.tls_certificate.clone(), &csp)?;
    if node_pks.idkg_dealing_encryption_pk.is_some() {
        ensure_idkg_dealing_encryption_key_is_set_up_locally(
            node_pks.idkg_dealing_encryption_pk.clone(),
            &csp,
        )?;
    }
    Ok(Some(node_pks))
}

fn node_public_keys_are_empty(node_pks: &NodePublicKeys) -> bool {
    node_pks.node_signing_pk.is_none()
        && node_pks.committee_signing_pk.is_none()
        && node_pks.dkg_dealing_encryption_pk.is_none()
        && node_pks.idkg_dealing_encryption_pk.is_none()
        && node_pks.tls_certificate.is_none()
}

fn ensure_node_signing_key_is_set_up_locally(
    maybe_pk_proto: Option<PublicKeyProto>,
    csp: &dyn CspSecretKeyStoreChecker,
) -> CryptoResult<()> {
    let pk_proto = maybe_pk_proto.ok_or_else(|| CryptoError::MalformedPublicKey {
        algorithm: AlgorithmId::Placeholder,
        key_bytes: None,
        internal_error: "missing node signing key in local public key store".to_string(),
    })?;
    ensure_node_signing_key_material_is_set_up_correctly(pk_proto, csp)?;
    Ok(())
}

fn ensure_committee_signing_key_is_set_up_locally(
    maybe_pk_proto: Option<PublicKeyProto>,
    csp: &dyn CspSecretKeyStoreChecker,
) -> CryptoResult<()> {
    let pk_proto = maybe_pk_proto.ok_or_else(|| CryptoError::MalformedPublicKey {
        algorithm: AlgorithmId::MultiBls12_381,
        key_bytes: None,
        internal_error: "missing committee signing key in local public key store".to_string(),
    })?;
    ensure_committee_signing_key_material_is_set_up_correctly(pk_proto, csp)?;
    Ok(())
}

fn ensure_dkg_dealing_encryption_key_is_set_up_locally(
    maybe_pk_proto: Option<PublicKeyProto>,
    csp: &dyn CspSecretKeyStoreChecker,
) -> CryptoResult<()> {
    let pk_proto = maybe_pk_proto.ok_or_else(|| CryptoError::MalformedPublicKey {
        algorithm: AlgorithmId::Groth20_Bls12_381,
        key_bytes: None,
        internal_error: "missing NI-DKG dealing encryption key in local public key store"
            .to_string(),
    })?;
    ensure_dkg_dealing_encryption_key_material_is_set_up_correctly(pk_proto, csp)?;
    Ok(())
}

fn ensure_idkg_dealing_encryption_key_is_set_up_locally(
    maybe_pk_proto: Option<PublicKeyProto>,
    csp: &dyn CspSecretKeyStoreChecker,
) -> CryptoResult<()> {
    let pk_proto = maybe_pk_proto.ok_or_else(|| CryptoError::MalformedPublicKey {
        algorithm: AlgorithmId::MegaSecp256k1,
        key_bytes: None,
        internal_error: "missing iDKG dealing encryption key in local public key store".to_string(),
    })?;
    ensure_idkg_dealing_encryption_key_material_is_set_up_correctly(pk_proto, csp)?;
    Ok(())
}

fn ensure_tls_cert_is_set_up_locally(
    maybe_tls_cert_proto: Option<X509PublicKeyCert>,
    csp: &dyn CspSecretKeyStoreChecker,
) -> CryptoResult<()> {
    let tls_cert_proto = maybe_tls_cert_proto.ok_or_else(|| CryptoError::MalformedPublicKey {
        algorithm: AlgorithmId::Tls,
        key_bytes: None,
        internal_error: "missing TLS public key certificate in local public key store".to_string(),
    })?;
    ensure_tls_key_material_is_set_up_correctly(tls_cert_proto, csp)?;
    Ok(())
}

fn generate_committee_signing_keys(csp: &dyn CryptoServiceProvider) -> PublicKeyProto {
    let generated = csp
        .gen_key_pair_with_pop(AlgorithmId::MultiBls12_381)
        .expect("Could not generate committee signing keys");
    match generated {
        (CspPublicKey::MultiBls12_381(pk_bytes), CspPop::MultiBls12_381(pop_bytes)) => {
            PublicKeyProto {
                algorithm: AlgorithmIdProto::MultiBls12381 as i32,
                key_value: pk_bytes.0.to_vec(),
                version: 0,
                proof_data: Some(pop_bytes.0.to_vec()),
            }
        }
        _ => panic!("Unexpected types"),
    }
}

/// Generates TLS key material for a `node`.
///
/// The secret key is stored in the key store of the provided `csp`,
/// and is used to create a self-signed public key certificate returned by this function.
///
/// The certificate's notAfter date indicates according to RFC5280 (section
/// 4.1.2.5; see https://tools.ietf.org/html/rfc5280#section-4.1.2.5) that the
/// certificate has no well-defined expiration date.
fn generate_tls_keys(csp: &mut dyn CryptoServiceProvider, node: NodeId) -> TlsPublicKeyCert {
    csp.gen_tls_key_pair(node, "99991231235959Z")
        .expect("error generating TLS key pair")
}

pub(crate) fn csp_for_config(
    config: &CryptoConfig,
    tokio_runtime_handle: Option<tokio::runtime::Handle>,
) -> Csp<OsRng, ProtoSecretKeyStore, ProtoSecretKeyStore> {
    Csp::new(
        config,
        tokio_runtime_handle,
        None,
        Arc::new(CryptoMetrics::none()),
    )
}

#[cfg(test)]
pub(crate) fn local_csp_in_temp_dir() -> (
    Csp<OsRng, ProtoSecretKeyStore, ProtoSecretKeyStore>,
    TempDir,
) {
    let (config, temp_dir) = CryptoConfig::new_in_temp_dir();
    let csp = csp_for_config(&config, None);
    (csp, temp_dir)
}
