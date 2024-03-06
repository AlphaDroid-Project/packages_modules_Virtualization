// Copyright 2023, The Android Open Source Project
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! This module contains functions related to the attestation of the
//! service VM via the RKP (Remote Key Provisioning) server.

use crate::keyblob::EncryptedKeyBlob;
use crate::pub_key::{build_maced_public_key, validate_public_key};
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use bssl_avf::EcKey;
use ciborium::{cbor, value::Value};
use core::result;
use coset::{iana, AsCborValue, CoseSign1, CoseSign1Builder, HeaderBuilder};
use diced_open_dice::{derive_cdi_leaf_priv, kdf, sign, DiceArtifacts, PrivateKey};
use log::error;
use service_vm_comm::{EcdsaP256KeyPair, GenerateCertificateRequestParams, RequestProcessingError};
use zeroize::Zeroizing;

type Result<T> = result::Result<T, RequestProcessingError>;

/// The salt is generated randomly with:
/// hexdump -vn32 -e'16/1 "0x%02X, " 1 "\n"' /dev/urandom
const HMAC_KEY_SALT: [u8; 32] = [
    0x82, 0x80, 0xFA, 0xD3, 0xA8, 0x0A, 0x9A, 0x4B, 0xF7, 0xA5, 0x7D, 0x7B, 0xE9, 0xC3, 0xAB, 0x13,
    0x89, 0xDC, 0x7B, 0x46, 0xEE, 0x71, 0x22, 0xB4, 0x5F, 0x4C, 0x3F, 0xE2, 0x40, 0x04, 0x3B, 0x6C,
];
const HMAC_KEY_INFO: &[u8] = b"rialto hmac wkey";
const HMAC_KEY_LENGTH: usize = 32;

pub(super) fn generate_ecdsa_p256_key_pair(
    dice_artifacts: &dyn DiceArtifacts,
) -> Result<EcdsaP256KeyPair> {
    let hmac_key = derive_hmac_key(dice_artifacts)?;
    let mut ec_key = EcKey::new_p256()?;
    ec_key.generate_key()?;

    let maced_public_key = build_maced_public_key(ec_key.cose_public_key()?, hmac_key.as_ref())?;
    let key_blob =
        EncryptedKeyBlob::new(ec_key.ec_private_key()?.as_slice(), dice_artifacts.cdi_seal())?;

    let key_pair =
        EcdsaP256KeyPair { maced_public_key, key_blob: cbor_util::serialize(&key_blob)? };
    Ok(key_pair)
}

const CSR_PAYLOAD_SCHEMA_V3: u8 = 3;
const AUTH_REQ_SCHEMA_V1: u8 = 1;
// TODO(b/300624493): Add a new certificate type for AVF CSR.
const CERTIFICATE_TYPE: &str = "keymint";

/// Builds the CSR described in:
///
/// hardware/interfaces/security/rkp/aidl/android/hardware/security/keymint/
/// generateCertificateRequestV2.cddl
pub(super) fn generate_certificate_request(
    params: GenerateCertificateRequestParams,
    dice_artifacts: &dyn DiceArtifacts,
) -> Result<Vec<u8>> {
    let hmac_key = derive_hmac_key(dice_artifacts)?;
    let mut public_keys: Vec<Value> = Vec::new();
    for key_to_sign in params.keys_to_sign {
        let public_key = validate_public_key(&key_to_sign, hmac_key.as_ref())?;
        public_keys.push(public_key.to_cbor_value()?);
    }
    // Builds `CsrPayload`.
    // TODO(b/299256925): The device information is currently empty as we do not
    // have sufficient details to include.
    let device_info = Value::Map(Vec::new());
    let csr_payload = cbor!([
        Value::Integer(CSR_PAYLOAD_SCHEMA_V3.into()),
        Value::Text(String::from(CERTIFICATE_TYPE)),
        device_info,
        Value::Array(public_keys),
    ])?;
    let csr_payload = cbor_util::serialize(&csr_payload)?;

    // Builds `SignedData`.
    let signed_data_payload =
        cbor!([Value::Bytes(params.challenge.to_vec()), Value::Bytes(csr_payload)])?;
    let signed_data = build_signed_data(&signed_data_payload, dice_artifacts)?.to_cbor_value()?;

    // Builds `AuthenticatedRequest<CsrPayload>`.
    // Currently `UdsCerts` is left empty because it is only needed for Samsung devices.
    // Check http://b/301574013#comment3 for more information.
    let uds_certs = Value::Map(Vec::new());
    let dice_cert_chain = dice_artifacts.bcc().ok_or(RequestProcessingError::MissingDiceChain)?;
    let dice_cert_chain: Value = cbor_util::deserialize(dice_cert_chain)?;
    let auth_req = cbor!([
        Value::Integer(AUTH_REQ_SCHEMA_V1.into()),
        uds_certs,
        dice_cert_chain,
        signed_data,
    ])?;
    Ok(cbor_util::serialize(&auth_req)?)
}

fn derive_hmac_key(dice_artifacts: &dyn DiceArtifacts) -> Result<Zeroizing<[u8; HMAC_KEY_LENGTH]>> {
    let mut key = Zeroizing::new([0u8; HMAC_KEY_LENGTH]);
    kdf(dice_artifacts.cdi_seal(), &HMAC_KEY_SALT, HMAC_KEY_INFO, key.as_mut()).map_err(|e| {
        error!("Failed to compute the HMAC key: {e}");
        RequestProcessingError::InternalError
    })?;
    Ok(key)
}

/// Builds the `SignedData` for the given payload.
fn build_signed_data(payload: &Value, dice_artifacts: &dyn DiceArtifacts) -> Result<CoseSign1> {
    let cdi_leaf_priv = derive_cdi_leaf_priv(dice_artifacts).map_err(|e| {
        error!("Failed to derive the CDI_Leaf_Priv: {e}");
        RequestProcessingError::InternalError
    })?;
    let signing_algorithm = iana::Algorithm::EdDSA;
    let protected = HeaderBuilder::new().algorithm(signing_algorithm).build();
    let signed_data = CoseSign1Builder::new()
        .protected(protected)
        .payload(cbor_util::serialize(payload)?)
        .try_create_signature(&[], |message| sign_message(message, &cdi_leaf_priv))?
        .build();
    Ok(signed_data)
}

fn sign_message(message: &[u8], private_key: &PrivateKey) -> Result<Vec<u8>> {
    Ok(sign(message, private_key.as_array())
        .map_err(|e| {
            error!("Failed to sign the CSR: {e}");
            RequestProcessingError::InternalError
        })?
        .to_vec())
}
