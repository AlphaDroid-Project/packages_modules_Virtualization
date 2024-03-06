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

//! This module mirrors the content in open-dice/include/dice/ops.h
//! It contains the set of functions that implement various operations that the
//! main DICE functions depend on.

use crate::dice::{
    derive_cdi_private_key_seed, DiceArtifacts, Hash, InputValues, PrivateKey, PublicKey,
    Signature, HASH_SIZE, PRIVATE_KEY_SEED_SIZE, PRIVATE_KEY_SIZE, PUBLIC_KEY_SIZE, SIGNATURE_SIZE,
};
use crate::error::{check_result, Result};
use open_dice_cbor_bindgen::{
    DiceGenerateCertificate, DiceHash, DiceKdf, DiceKeypairFromSeed, DiceSign, DiceVerify,
};
use std::ptr;

/// Hashes the provided input using DICE's hash function `DiceHash`.
pub fn hash(input: &[u8]) -> Result<Hash> {
    let mut output: Hash = [0; HASH_SIZE];
    check_result(
        // SAFETY: DiceHash takes a sized input buffer and writes to a constant-sized output
        // buffer. The first argument context is not used in this function.
        unsafe {
            DiceHash(
                ptr::null_mut(), // context
                input.as_ptr(),
                input.len(),
                output.as_mut_ptr(),
            )
        },
        output.len(),
    )?;
    Ok(output)
}

/// An implementation of HKDF-SHA512. Derives a key of `derived_key.len()` bytes from `ikm`, `salt`,
/// and `info`. The derived key is written to the `derived_key`.
pub fn kdf(ikm: &[u8], salt: &[u8], info: &[u8], derived_key: &mut [u8]) -> Result<()> {
    check_result(
        // SAFETY: The function writes to the `derived_key`, within the given bounds, and only
        // reads the input values. The first argument context is not used in this function.
        unsafe {
            DiceKdf(
                ptr::null_mut(), // context
                derived_key.len(),
                ikm.as_ptr(),
                ikm.len(),
                salt.as_ptr(),
                salt.len(),
                info.as_ptr(),
                info.len(),
                derived_key.as_mut_ptr(),
            )
        },
        derived_key.len(),
    )
}

/// Deterministically generates a public and private key pair from `seed`.
/// Since this is deterministic, `seed` is as sensitive as a private key and can
/// be used directly as the private key.
pub fn keypair_from_seed(seed: &[u8; PRIVATE_KEY_SEED_SIZE]) -> Result<(PublicKey, PrivateKey)> {
    let mut public_key = [0u8; PUBLIC_KEY_SIZE];
    let mut private_key = PrivateKey::default();
    check_result(
        // SAFETY: The function writes to the `public_key` and `private_key` within the given
        // bounds, and only reads the `seed`. The first argument context is not used in this
        // function.
        unsafe {
            DiceKeypairFromSeed(
                ptr::null_mut(), // context
                seed.as_ptr(),
                public_key.as_mut_ptr(),
                private_key.as_mut_ptr(),
            )
        },
        public_key.len(),
    )?;
    Ok((public_key, private_key))
}

/// Derives the CDI_Leaf_Priv from the provided `dice_artifacts`.
///
/// The corresponding public key is included in the leaf certificate of the DICE chain
/// contained in `dice_artifacts`.
///
/// Refer to the following documentation for more information about CDI_Leaf_Priv:
///
/// security/rkp/aidl/android/hardware/security/keymint/IRemotelyProvisionedComponent.aidl
pub fn derive_cdi_leaf_priv(dice_artifacts: &dyn DiceArtifacts) -> Result<PrivateKey> {
    let cdi_priv_key_seed = derive_cdi_private_key_seed(dice_artifacts.cdi_attest())?;
    let (_, private_key) = keypair_from_seed(cdi_priv_key_seed.as_array())?;
    Ok(private_key)
}

/// Signs the `message` with the give `private_key` using `DiceSign`.
pub fn sign(message: &[u8], private_key: &[u8; PRIVATE_KEY_SIZE]) -> Result<Signature> {
    let mut signature = [0u8; SIGNATURE_SIZE];
    check_result(
        // SAFETY: The function writes to the `signature` within the given bounds, and only reads
        // the message and the private key. The first argument context is not used in this
        // function.
        unsafe {
            DiceSign(
                ptr::null_mut(), // context
                message.as_ptr(),
                message.len(),
                private_key.as_ptr(),
                signature.as_mut_ptr(),
            )
        },
        signature.len(),
    )?;
    Ok(signature)
}

/// Verifies the `signature` of the `message` with the given `public_key` using `DiceVerify`.
pub fn verify(message: &[u8], signature: &Signature, public_key: &PublicKey) -> Result<()> {
    check_result(
        // SAFETY: only reads the messages, signature and public key as constant values.
        // The first argument context is not used in this function.
        unsafe {
            DiceVerify(
                ptr::null_mut(), // context
                message.as_ptr(),
                message.len(),
                signature.as_ptr(),
                public_key.as_ptr(),
            )
        },
        0,
    )
}

/// Generates an X.509 certificate from the given `subject_private_key_seed` and
/// `input_values`, and signed by `authority_private_key_seed`.
/// The subject private key seed is supplied here so the implementation can choose
/// between asymmetric mechanisms, for example ECDSA vs Ed25519.
/// Returns the actual size of the generated certificate.
pub fn generate_certificate(
    subject_private_key_seed: &[u8; PRIVATE_KEY_SEED_SIZE],
    authority_private_key_seed: &[u8; PRIVATE_KEY_SEED_SIZE],
    input_values: &InputValues,
    certificate: &mut [u8],
) -> Result<usize> {
    let mut certificate_actual_size = 0;
    check_result(
        // SAFETY: The function writes to the `certificate` within the given bounds, and only reads
        // the input values and the key seeds. The first argument context is not used in this
        // function.
        unsafe {
            DiceGenerateCertificate(
                ptr::null_mut(), // context
                subject_private_key_seed.as_ptr(),
                authority_private_key_seed.as_ptr(),
                input_values.as_ptr(),
                certificate.len(),
                certificate.as_mut_ptr(),
                &mut certificate_actual_size,
            )
        },
        certificate_actual_size,
    )?;
    Ok(certificate_actual_size)
}
