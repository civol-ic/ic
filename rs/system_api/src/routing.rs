use std::str::FromStr;

use candid::Decode;
use ic_base_types::{CanisterId, SubnetId};
use ic_ic00_types::{
    CanisterIdRecord, ComputeInitialEcdsaDealingsArgs, ECDSAPublicKeyArgs, EcdsaKeyId,
    InstallCodeArgs, Method as Ic00Method, Payload, ProvisionalTopUpCanisterArgs,
    SetControllerArgs, SignWithECDSAArgs, UpdateSettingsArgs,
};
use ic_replicated_state::NetworkTopology;

#[derive(Debug)]
pub(super) enum ResolveDestinationError {
    CandidError(candid::Error),
    MethodNotFound(String),
    SubnetNotFound(CanisterId, Ic00Method),
}

impl From<candid::Error> for ResolveDestinationError {
    fn from(err: candid::Error) -> Self {
        ResolveDestinationError::CandidError(err)
    }
}

/// Inspect the method name and payload of a request to ic:00 to figure out to
/// which subnet it should be sent to.
pub(super) fn resolve_destination(
    network_topology: &NetworkTopology,
    method_name: &str,
    payload: &[u8],
    own_subnet: SubnetId,
) -> Result<SubnetId, ResolveDestinationError> {
    // Figure out the destination subnet based on the method and the payload.
    let method = Ic00Method::from_str(method_name);
    match method {
        Ok(Ic00Method::CreateCanister)
        | Ok(Ic00Method::RawRand)
        | Ok(Ic00Method::ProvisionalCreateCanisterWithCycles)
        | Ok(Ic00Method::HttpRequest) => Ok(own_subnet),
        // This message needs to be routed to the NNS subnet.  We assume that
        // this message can only be sent by canisters on the NNS subnet hence
        // returning `own_subnet` here is fine.
        //
        // It might be cleaner to pipe in the actual NNS subnet id to this
        // function and return that instead.
        Ok(Ic00Method::SetupInitialDKG) => Ok(own_subnet),
        Ok(Ic00Method::UpdateSettings) => {
            // Find the destination canister from the payload.
            let args = Decode!(payload, UpdateSettingsArgs)?;
            let canister_id = args.get_canister_id();
            network_topology
                .routing_table
                .route(canister_id.get())
                .ok_or({
                    ResolveDestinationError::SubnetNotFound(canister_id, Ic00Method::UpdateSettings)
                })
        }
        Ok(Ic00Method::InstallCode) => {
            // Find the destination canister from the payload.
            let args = Decode!(payload, InstallCodeArgs)?;
            let canister_id = args.get_canister_id();
            network_topology
                .routing_table
                .route(canister_id.get())
                .ok_or({
                    ResolveDestinationError::SubnetNotFound(canister_id, Ic00Method::InstallCode)
                })
        }
        Ok(Ic00Method::SetController) => {
            let args = Decode!(payload, SetControllerArgs)?;
            let canister_id = args.get_canister_id();
            network_topology
                .routing_table
                .route(canister_id.get())
                .ok_or({
                    ResolveDestinationError::SubnetNotFound(canister_id, Ic00Method::SetController)
                })
        }
        Ok(Ic00Method::CanisterStatus)
        | Ok(Ic00Method::StartCanister)
        | Ok(Ic00Method::StopCanister)
        | Ok(Ic00Method::DeleteCanister)
        | Ok(Ic00Method::UninstallCode)
        | Ok(Ic00Method::DepositCycles) => {
            let args = Decode!(payload, CanisterIdRecord)?;
            let canister_id = args.get_canister_id();
            network_topology
                .routing_table
                .route(canister_id.get())
                .ok_or_else(|| {
                    ResolveDestinationError::SubnetNotFound(canister_id, method.unwrap())
                })
        }
        Ok(Ic00Method::ProvisionalTopUpCanister) => {
            let args = ProvisionalTopUpCanisterArgs::decode(payload)?;
            let canister_id = args.get_canister_id();
            network_topology
                .routing_table
                .route(canister_id.get())
                .ok_or({
                    ResolveDestinationError::SubnetNotFound(
                        canister_id,
                        Ic00Method::ProvisionalTopUpCanister,
                    )
                })
        }
        Ok(Ic00Method::BitcoinGetBalance)
        | Ok(Ic00Method::BitcoinGetUtxos)
        | Ok(Ic00Method::BitcoinSendTransaction)
        | Ok(Ic00Method::BitcoinGetCurrentFeePercentiles) => {
            // TODO(EXC-939): Route requests across all the bitcoin subnets, not only
            // the first subnet.
            Ok(*network_topology
                .bitcoin_testnet_subnets()
                .first()
                .unwrap_or(&own_subnet))
        }
        Ok(Ic00Method::ECDSAPublicKey) => {
            let key_id = Decode!(payload, ECDSAPublicKeyArgs)?.key_id;
            route_ecdsa_message(
                &key_id,
                network_topology,
                &None,
                EcdsaSubnetKind::OnlyHoldsKey,
            )
        }
        Ok(Ic00Method::SignWithECDSA) => {
            let key_id = Decode!(payload, SignWithECDSAArgs)?.key_id;
            route_ecdsa_message(
                &key_id,
                network_topology,
                &None,
                EcdsaSubnetKind::HoldsAndSignWithKey,
            )
        }
        Ok(Ic00Method::ComputeInitialEcdsaDealings) => {
            let args = Decode!(payload, ComputeInitialEcdsaDealingsArgs)?;
            route_ecdsa_message(
                &args.key_id,
                network_topology,
                &Some(args.subnet_id),
                EcdsaSubnetKind::OnlyHoldsKey,
            )
        }
        Err(_) => Err(ResolveDestinationError::MethodNotFound(
            method_name.to_string(),
        )),
    }
}

enum EcdsaSubnetKind {
    OnlyHoldsKey,
    HoldsAndSignWithKey,
}

/// Routes to the `requested_subnet` if it holds the key (and fails if that
/// subnet doesn't hold the key).  If a `requested_subnet` is not provided,
/// route to the first subnet enabled to sign with the given key.
fn route_ecdsa_message(
    key_id: &EcdsaKeyId,
    network_topology: &NetworkTopology,
    requested_subnet: &Option<SubnetId>,
    signing_must_be_enabled: EcdsaSubnetKind,
) -> Result<SubnetId, ResolveDestinationError> {
    match requested_subnet {
        Some(subnet_id) => match network_topology.subnets.get(subnet_id) {
            None => Err(ResolveDestinationError::MethodNotFound(format!(
                "Requested ECDSA key {} from unknown subnet {}",
                key_id, subnet_id
            ))),
            Some(subnet_topology) => {
                if subnet_topology.ecdsa_keys_held.contains(key_id) {
                    Ok(*subnet_id)
                } else {
                    Err(ResolveDestinationError::MethodNotFound(format!(
                        "Requested ECDSA key {} on subnet {}, subnet has keys: {:?}",
                        key_id, subnet_id, subnet_topology.ecdsa_keys_held
                    )))
                }
            }
        },
        None => {
            // If some subnet is enabled to sign for the key we can immediately return it.
            if let Some(subnet_id) = network_topology.ecdsa_signing_subnets(key_id).get(0) {
                return Ok(*subnet_id);
            }
            // Otherwise either return an error, or look through all subnets to
            // find one with the key if signing isn't required.
            match signing_must_be_enabled {
                EcdsaSubnetKind::HoldsAndSignWithKey => {
                    Err(ResolveDestinationError::MethodNotFound(format!(
                        "Requested ECDSA key: {}, existing keys with signing enabled: {:?}",
                        key_id, network_topology.ecdsa_signing_subnets
                    )))
                }
                EcdsaSubnetKind::OnlyHoldsKey => {
                    for (subnet_id, topology) in &network_topology.subnets {
                        if topology.ecdsa_keys_held.contains(key_id) {
                            return Ok(*subnet_id);
                        }
                    }
                    Err(ResolveDestinationError::MethodNotFound(format!(
                        "No subnet was found with requested ECDSA key: {}",
                        key_id
                    )))
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use assert_matches::assert_matches;
    use candid::Encode;
    use ic_base_types::RegistryVersion;
    use ic_ic00_types::{
        ComputeInitialEcdsaDealingsArgs, EcdsaCurve, EcdsaKeyId, SignWithECDSAArgs,
    };
    use ic_replicated_state::SubnetTopology;
    use ic_test_utilities::types::ids::{canister_test_id, node_test_id, subnet_test_id};
    use maplit::btreemap;

    use super::*;

    fn key_id1() -> EcdsaKeyId {
        EcdsaKeyId {
            curve: EcdsaCurve::Secp256k1,
            name: "some_key".to_string(),
        }
    }

    fn key_id2() -> EcdsaKeyId {
        EcdsaKeyId {
            curve: EcdsaCurve::Secp256k1,
            name: "other_key".to_string(),
        }
    }

    /// Two subnets have key_id1, but only one of the subnets is enabled
    /// to sign with it.
    /// Only one subnet has key_id2, and it isn't enabled to sign with it.
    fn network_with_ecdsa_subnets() -> NetworkTopology {
        let subnet_id0 = subnet_test_id(0);
        NetworkTopology {
            // Only subnet 0 can sign with the first key.
            ecdsa_signing_subnets: btreemap! {
                key_id1() => vec![subnet_id0],
            },
            subnets: btreemap! {
                // Subnet 0 holds both keys
                subnet_id0 => SubnetTopology {
                    ecdsa_keys_held: vec![key_id1(), key_id2()].into_iter().collect(),
                    ..SubnetTopology::default()
                },
                // Subnet 1 holds only the first key.
                subnet_test_id(1) => SubnetTopology {
                    ecdsa_keys_held: vec![key_id1()].into_iter().collect(),
                    ..SubnetTopology::default()
                },
                subnet_test_id(2) => SubnetTopology::default(),
            },
            ..NetworkTopology::default()
        }
    }

    fn network_without_ecdsa_subnet() -> NetworkTopology {
        NetworkTopology::default()
    }

    fn compute_initial_ecdsa_dealings_req(key_id: EcdsaKeyId, subnet_id: SubnetId) -> Vec<u8> {
        let args = ComputeInitialEcdsaDealingsArgs::new(
            key_id,
            subnet_id,
            vec![node_test_id(0)].into_iter().collect(),
            RegistryVersion::from(100),
        );
        Encode!(&args).unwrap()
    }

    fn ecdsa_sign_req(key_id: EcdsaKeyId) -> Vec<u8> {
        let args = SignWithECDSAArgs {
            message_hash: [1; 32],
            derivation_path: vec![vec![0; 10]],
            key_id,
        };
        Encode!(&args).unwrap()
    }

    fn public_key_req(key_id: EcdsaKeyId) -> Vec<u8> {
        let args = ECDSAPublicKeyArgs {
            canister_id: Some(canister_test_id(1)),
            derivation_path: vec![vec![0; 10]],
            key_id,
        };
        Encode!(&args).unwrap()
    }

    #[test]
    fn resolve_compute_initial_ecdsa_dealings() {
        assert_eq!(
            resolve_destination(
                &network_with_ecdsa_subnets(),
                &Ic00Method::ComputeInitialEcdsaDealings.to_string(),
                &compute_initial_ecdsa_dealings_req(key_id1(), subnet_test_id(1)),
                subnet_test_id(2),
            )
            .unwrap(),
            subnet_test_id(1)
        )
    }

    #[test]
    fn resolve_compute_initial_ecdsa_dealings_key_not_held_error() {
        assert_matches!(
            resolve_destination(
                &network_with_ecdsa_subnets(),
                &Ic00Method::ComputeInitialEcdsaDealings.to_string(),
                &compute_initial_ecdsa_dealings_req(key_id1(), subnet_test_id(2)),
                subnet_test_id(2),
            )
            .unwrap_err(),
            ResolveDestinationError::MethodNotFound(err) => assert_eq!(
                err,
                format!("Requested ECDSA key {} on subnet {}, subnet has keys: {{}}", key_id1(), subnet_test_id(2))
            )
        )
    }

    #[test]
    fn resolve_compute_initial_ecdsa_dealings_unknown_subnet_error() {
        assert_matches!(
            resolve_destination(
                &network_with_ecdsa_subnets(),
                &Ic00Method::ComputeInitialEcdsaDealings.to_string(),
                &compute_initial_ecdsa_dealings_req(key_id1(), subnet_test_id(3)),
                subnet_test_id(2),
            )
            .unwrap_err(),
            ResolveDestinationError::MethodNotFound(err) => assert_eq!(
                err,
                format!("Requested ECDSA key {} from unknown subnet {}", key_id1(), subnet_test_id(3))
            )
        )
    }

    #[test]
    fn resolve_compute_initial_ecdsa_dealings_wrong_subnet_error() {
        assert_matches!(
                resolve_destination(
                    &network_with_ecdsa_subnets(),
                    &Ic00Method::ComputeInitialEcdsaDealings.to_string(),
                    // Subnet 2 doesn't have the requested key.
                    &compute_initial_ecdsa_dealings_req(key_id1(), subnet_test_id(2)),
                    subnet_test_id(2),
                )
                .unwrap_err(),
                ResolveDestinationError::MethodNotFound(err) => assert_eq!(
                    err,
                    format!("Requested ECDSA key {} on subnet {}, subnet has keys: {{}}",
                        key_id1(),
                        subnet_test_id(2),
                )
            )
        )
    }

    #[test]
    fn resolve_compute_initial_ecdsa_dealings_subnet_not_found_error() {
        assert_matches!(
                resolve_destination(
                    &network_with_ecdsa_subnets(),
                    &Ic00Method::ComputeInitialEcdsaDealings.to_string(),
                    // Subnet 3 doesn't exist
                    &compute_initial_ecdsa_dealings_req(key_id1(), subnet_test_id(3)),
                    subnet_test_id(2),
                )
                .unwrap_err(),
                ResolveDestinationError::MethodNotFound(err) => assert_eq!(
                    err,
                    format!("Requested ECDSA key {} from unknown subnet {}",
                        key_id1(),
                        subnet_test_id(3),
                )
            )
        )
    }

    #[test]
    fn resolve_ecdsa_sign() {
        assert_eq!(
            resolve_destination(
                &network_with_ecdsa_subnets(),
                &Ic00Method::SignWithECDSA.to_string(),
                &ecdsa_sign_req(key_id1()),
                subnet_test_id(1),
            )
            .unwrap(),
            subnet_test_id(0)
        )
    }

    #[test]
    fn resolve_ecdsa_sign_error() {
        assert_matches!(resolve_destination(
            &network_without_ecdsa_subnet(),
            &Ic00Method::SignWithECDSA.to_string(),
            &ecdsa_sign_req(key_id1()),
            subnet_test_id(1),
        )
        .unwrap_err(),
        ResolveDestinationError::MethodNotFound(err) => assert_eq!(
                err,
                format!("Requested ECDSA key: {}, existing keys with signing enabled: {{}}", key_id1())
            )
        )
    }

    #[test]
    fn resolve_ecdsa_public_key_works_without_signing_enabled() {
        assert_eq!(
            resolve_destination(
                &network_with_ecdsa_subnets(),
                &Ic00Method::ECDSAPublicKey.to_string(),
                &public_key_req(key_id2()),
                subnet_test_id(1),
            )
            .unwrap(),
            subnet_test_id(0)
        )
    }

    #[test]
    fn resolve_ecdsa_initial_dealings_works_without_signing_enabled() {
        assert_eq!(
            resolve_destination(
                &network_with_ecdsa_subnets(),
                &Ic00Method::ComputeInitialEcdsaDealings.to_string(),
                &compute_initial_ecdsa_dealings_req(key_id2(), subnet_test_id(0)),
                subnet_test_id(1),
            )
            .unwrap(),
            subnet_test_id(0)
        )
    }
}
