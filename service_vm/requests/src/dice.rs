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

//! This module contains functions related to DICE.

use alloc::string::String;
use alloc::vec::Vec;
use bssl_avf::{ed25519_verify, Digester, EcKey};
use cbor_util::{
    cbor_value_type, get_label_value, get_label_value_as_bytes, value_to_array,
    value_to_byte_array, value_to_bytes, value_to_map, value_to_num, value_to_text,
};
use ciborium::value::Value;
use core::cell::OnceCell;
use core::result;
use coset::{
    self,
    iana::{self, EnumI64},
    Algorithm, AsCborValue, CborSerializable, CoseError, CoseKey, CoseSign1, KeyOperation, KeyType,
    Label,
};
use diced_open_dice::{DiceMode, HASH_SIZE};
use log::error;
use service_vm_comm::RequestProcessingError;

type Result<T> = result::Result<T, RequestProcessingError>;

const CODE_HASH: i64 = -4670545;
const CONFIG_DESC: i64 = -4670548;
const AUTHORITY_HASH: i64 = -4670549;
const MODE: i64 = -4670551;
const SUBJECT_PUBLIC_KEY: i64 = -4670552;

const CONFIG_DESC_COMPONENT_NAME: i64 = -70002;
const CONFIG_DESC_SUB_COMPONENTS: i64 = -71002;

const SUB_COMPONENT_NAME: i64 = 1;
const SUB_COMPONENT_VERSION: i64 = 2;
const SUB_COMPONENT_CODE_HASH: i64 = 3;
const SUB_COMPONENT_AUTHORITY_HASH: i64 = 4;

const MICRODROID_KERNEL_COMPONENT_NAME: &str = "vm_entry";
const MICRODROID_PAYLOAD_COMPONENT_NAME: &str = "Microdroid payload";

/// Represents a partially decoded `DiceCertChain` from the client VM.
/// The whole chain is defined as following:
///
/// DiceCertChain = [
///     PubKeyEd25519 / PubKeyECDSA256 / PubKeyECDSA384,  ; UDS_Pub
///     + DiceChainEntry,               ; First CDI_Certificate -> Last CDI_Certificate
/// ]
#[derive(Debug, Clone)]
pub(crate) struct ClientVmDiceChain {
    payloads: Vec<DiceChainEntryPayload>,
}

impl ClientVmDiceChain {
    /// Validates the signatures of the entries in the `client_vm_dice_chain` as following:
    ///
    /// - The first entry of the `client_vm_dice_chain` must be signed with the root public key.
    /// - After the first entry, each entry of the `client_vm_dice_chain` must be signed with the
    ///  subject public key of the previous entry.
    ///
    /// Returns a partially decoded client VM's DICE chain if the verification succeeds.
    pub(crate) fn validate_signatures_and_parse_dice_chain(
        mut client_vm_dice_chain: Vec<Value>,
    ) -> Result<Self> {
        let root_public_key =
            CoseKey::from_cbor_value(client_vm_dice_chain.remove(0))?.try_into()?;

        let mut payloads = Vec::with_capacity(client_vm_dice_chain.len());
        let mut previous_public_key = &root_public_key;
        for (i, value) in client_vm_dice_chain.into_iter().enumerate() {
            let payload = DiceChainEntryPayload::validate_cose_signature_and_extract_payload(
                value,
                previous_public_key,
            )
            .map_err(|e| {
                error!("Failed to verify the DICE chain entry {}: {:?}", i, e);
                e
            })?;
            payloads.push(payload);
            previous_public_key = &payloads.last().unwrap().subject_public_key;
        }
        // After successfully calling `validate_client_vm_dice_chain_prefix_match`, we can be
        // certain that the client VM's DICE chain must contain at least three entries that
        // describe:
        // - pvmfw
        // - Microdroid kernel
        // - Apk/Apexes
        assert!(
            payloads.len() >= 3,
            "The client VM DICE chain must contain at least three DiceChainEntryPayloads"
        );
        let chain = Self { payloads };
        chain.validate_microdroid_components_names()?;
        Ok(chain)
    }

    fn validate_microdroid_components_names(&self) -> Result<()> {
        let microdroid_kernel_name = &self.microdroid_kernel().config_descriptor.component_name;
        if MICRODROID_KERNEL_COMPONENT_NAME != microdroid_kernel_name {
            error!(
                "The second to last entry in the client VM DICE chain must describe the \
                    Microdroid kernel. Got {}",
                microdroid_kernel_name
            );
            return Err(RequestProcessingError::InvalidDiceChain);
        }
        let microdroid_payload_name = &self.microdroid_payload().config_descriptor.component_name;
        if MICRODROID_PAYLOAD_COMPONENT_NAME != microdroid_payload_name {
            error!(
                "The last entry in the client VM DICE chain must describe the Microdroid \
                    payload. Got {}",
                microdroid_payload_name
            );
            return Err(RequestProcessingError::InvalidDiceChain);
        }
        Ok(())
    }

    pub(crate) fn microdroid_kernel(&self) -> &DiceChainEntryPayload {
        &self.payloads[self.payloads.len() - 2]
    }

    pub(crate) fn microdroid_payload(&self) -> &DiceChainEntryPayload {
        &self.payloads[self.payloads.len() - 1]
    }

    pub(crate) fn microdroid_payload_components(&self) -> Option<&Vec<SubComponent>> {
        self.microdroid_payload().config_descriptor.sub_components.as_ref()
    }

    /// Returns true if all payloads in the DICE chain are in normal mode.
    pub(crate) fn all_entries_are_secure(&self) -> bool {
        self.payloads.iter().all(|p| p.mode == DiceMode::kDiceModeNormal)
    }
}

/// Validates that the `client_vm_dice_chain` matches the `service_vm_dice_chain` up to the pvmfw
/// entry.
///
/// Returns `Ok(())` if the verification succeeds.
pub(crate) fn validate_client_vm_dice_chain_prefix_match(
    client_vm_dice_chain: &[Value],
    service_vm_dice_chain: &[Value],
) -> Result<()> {
    if service_vm_dice_chain.len() < 3 {
        // The service VM's DICE chain must contain the root key and at least two other entries
        // that describe:
        //   - pvmfw
        //   - Service VM kernel
        error!("The service VM DICE chain must contain at least three entries");
        return Err(RequestProcessingError::InternalError);
    }
    // Ignores the last entry that describes service VM
    let entries_up_to_pvmfw = &service_vm_dice_chain[0..(service_vm_dice_chain.len() - 1)];
    if entries_up_to_pvmfw.len() + 2 != client_vm_dice_chain.len() {
        // Client VM DICE chain = entries_up_to_pvmfw
        //    + Microdroid kernel entry (added in pvmfw)
        //    + Apk/Apexes entry (added in microdroid)
        error!("The client VM's DICE chain must contain exactly two extra entries");
        return Err(RequestProcessingError::InvalidDiceChain);
    }
    if entries_up_to_pvmfw != &client_vm_dice_chain[0..entries_up_to_pvmfw.len()] {
        error!(
            "The client VM's DICE chain does not match service VM's DICE chain up to \
             the pvmfw entry"
        );
        return Err(RequestProcessingError::InvalidDiceChain);
    }
    Ok(())
}

#[derive(Debug, Clone)]
pub(crate) struct PublicKey(CoseKey);

impl TryFrom<CoseKey> for PublicKey {
    type Error = RequestProcessingError;

    fn try_from(key: CoseKey) -> Result<Self> {
        if !key.key_ops.contains(&KeyOperation::Assigned(iana::KeyOperation::Verify)) {
            error!("Public key does not support verification");
            return Err(RequestProcessingError::InvalidDiceChain);
        }
        Ok(Self(key))
    }
}

impl PublicKey {
    /// Verifies the signature of the provided message with the public key.
    ///
    /// This function supports the following key/algorithm types as specified in
    /// hardware/interfaces/security/rkp/aidl/android/hardware/security/keymint/
    /// generateCertificateRequestV2.cddl:
    ///
    /// PubKeyEd25519 / PubKeyECDSA256 / PubKeyECDSA384
    pub(crate) fn verify(&self, signature: &[u8], message: &[u8]) -> Result<()> {
        match &self.0.kty {
            KeyType::Assigned(iana::KeyType::EC2) => {
                let public_key = EcKey::from_cose_public_key(&self.0)?;
                let Some(Algorithm::Assigned(alg)) = self.0.alg else {
                    error!("Invalid algorithm in COSE key {:?}", self.0.alg);
                    return Err(RequestProcessingError::InvalidDiceChain);
                };
                let digester = match alg {
                    iana::Algorithm::ES256 => Digester::sha256(),
                    iana::Algorithm::ES384 => Digester::sha384(),
                    _ => {
                        error!("Unsupported algorithm in EC2 key: {:?}", alg);
                        return Err(RequestProcessingError::InvalidDiceChain);
                    }
                };
                let digest = digester.digest(message)?;
                Ok(public_key.ecdsa_verify(signature, &digest)?)
            }
            KeyType::Assigned(iana::KeyType::OKP) => {
                let curve_type =
                    get_label_value(&self.0, Label::Int(iana::OkpKeyParameter::Crv.to_i64()))?;
                if curve_type != &Value::from(iana::EllipticCurve::Ed25519.to_i64()) {
                    error!("Unsupported curve type in OKP COSE key: {:?}", curve_type);
                    return Err(RequestProcessingError::OperationUnimplemented);
                }
                let x = get_label_value_as_bytes(
                    &self.0,
                    Label::Int(iana::OkpKeyParameter::X.to_i64()),
                )?;
                let public_key = x.try_into().map_err(|_| {
                    error!("Invalid ED25519 public key size: {}", x.len());
                    RequestProcessingError::InvalidDiceChain
                })?;
                let signature = signature.try_into().map_err(|_| {
                    error!("Invalid ED25519 signature size: {}", signature.len());
                    RequestProcessingError::InvalidDiceChain
                })?;
                Ok(ed25519_verify(message, signature, public_key)?)
            }
            kty => {
                error!("Unsupported key type in COSE key: {:?}", kty);
                Err(RequestProcessingError::OperationUnimplemented)
            }
        }
    }
}

/// Represents a partially decoded `DiceChainEntryPayload`. The whole payload is defined in:
///
/// hardware/interfaces/security/rkp/aidl/android/hardware/security/keymint/
/// generateCertificateRequestV2.cddl
#[derive(Debug, Clone)]
pub(crate) struct DiceChainEntryPayload {
    pub(crate) subject_public_key: PublicKey,
    mode: DiceMode,
    pub(crate) code_hash: [u8; HASH_SIZE],
    pub(crate) authority_hash: [u8; HASH_SIZE],
    config_descriptor: ConfigDescriptor,
}

impl DiceChainEntryPayload {
    /// Validates the signature of the provided CBOR value with the provided public key and
    /// extracts payload from the value.
    fn validate_cose_signature_and_extract_payload(
        value: Value,
        authority_public_key: &PublicKey,
    ) -> Result<Self> {
        let cose_sign1 = CoseSign1::from_cbor_value(value)?;
        let aad = &[]; // AAD is not used in DICE chain entry.
        cose_sign1.verify_signature(aad, |signature, message| {
            authority_public_key.verify(signature, message)
        })?;

        let payload = cose_sign1.payload.ok_or_else(|| {
            error!("No payload found in the DICE chain entry");
            RequestProcessingError::InvalidDiceChain
        })?;
        Self::from_slice(&payload)
    }

    pub(crate) fn from_slice(data: &[u8]) -> Result<Self> {
        let entries = value_to_map(Value::from_slice(data)?, "DiceChainEntryPayload")?;
        let mut builder = PayloadBuilder::default();
        for (key, value) in entries.into_iter() {
            let key: i64 = value_to_num(key, "DiceChainEntryPayload key")?;
            match key {
                SUBJECT_PUBLIC_KEY => {
                    let subject_public_key = value_to_bytes(value, "subject_public_key")?;
                    let subject_public_key =
                        CoseKey::from_slice(&subject_public_key)?.try_into()?;
                    builder.subject_public_key(subject_public_key)?;
                }
                MODE => builder.mode(to_mode(value)?)?,
                CODE_HASH => {
                    let code_hash = value_to_byte_array(value, "DiceChainEntryPayload code_hash")?;
                    builder.code_hash(code_hash)?;
                }
                AUTHORITY_HASH => {
                    let authority_hash =
                        value_to_byte_array(value, "DiceChainEntryPayload authority_hash")?;
                    builder.authority_hash(authority_hash)?;
                }
                CONFIG_DESC => {
                    let config_descriptor = value_to_bytes(value, "config_descriptor")?;
                    let config_descriptor = ConfigDescriptor::from_slice(&config_descriptor)?;
                    builder.config_descriptor(config_descriptor)?;
                }
                _ => {}
            }
        }
        builder.build()
    }
}
/// Represents a partially decoded `ConfigurationDescriptor`.
///
/// The whole `ConfigurationDescriptor` is defined in:
///
/// hardware/interfaces/security/rkp/aidl/android/hardware/security/keymint/
/// generateCertificateRequestV2.cddl
#[derive(Debug, Clone)]
pub(crate) struct ConfigDescriptor {
    component_name: String,
    sub_components: Option<Vec<SubComponent>>,
}

impl ConfigDescriptor {
    fn from_slice(data: &[u8]) -> Result<Self> {
        let value = Value::from_slice(data)?;
        let entries = value_to_map(value, "ConfigDescriptor")?;
        let mut builder = ConfigDescriptorBuilder::default();
        for (key, value) in entries.into_iter() {
            let key: i64 = value_to_num(key, "ConfigDescriptor key")?;
            match key {
                CONFIG_DESC_COMPONENT_NAME => {
                    let name = value_to_text(value, "ConfigDescriptor component_name")?;
                    builder.component_name(name)?;
                }
                CONFIG_DESC_SUB_COMPONENTS => {
                    let sub_components = value_to_array(value, "ConfigDescriptor sub_components")?;
                    let sub_components = sub_components
                        .into_iter()
                        .map(SubComponent::try_from)
                        .collect::<Result<Vec<_>>>()?;
                    builder.sub_components(sub_components)?
                }
                _ => {}
            }
        }
        builder.build()
    }
}

#[derive(Debug, Clone, Default)]
struct ConfigDescriptorBuilder {
    component_name: OnceCell<String>,
    sub_components: OnceCell<Vec<SubComponent>>,
}

impl ConfigDescriptorBuilder {
    fn component_name(&mut self, component_name: String) -> Result<()> {
        set_once(&self.component_name, component_name, "ConfigDescriptor component_name")
    }

    fn sub_components(&mut self, sub_components: Vec<SubComponent>) -> Result<()> {
        set_once(&self.sub_components, sub_components, "ConfigDescriptor sub_components")
    }

    fn build(mut self) -> Result<ConfigDescriptor> {
        let component_name =
            take_value(&mut self.component_name, "ConfigDescriptor component_name")?;
        let sub_components = self.sub_components.take();
        Ok(ConfigDescriptor { component_name, sub_components })
    }
}

#[derive(Debug, Clone)]
pub(crate) struct SubComponent {
    pub(crate) name: String,
    pub(crate) version: u64,
    pub(crate) code_hash: Vec<u8>,
    pub(crate) authority_hash: Vec<u8>,
}

impl TryFrom<Value> for SubComponent {
    type Error = RequestProcessingError;

    fn try_from(value: Value) -> Result<Self> {
        let entries = value_to_map(value, "SubComponent")?;
        let mut builder = SubComponentBuilder::default();
        for (key, value) in entries.into_iter() {
            let key: i64 = value_to_num(key, "SubComponent key")?;
            match key {
                SUB_COMPONENT_NAME => {
                    builder.name(value_to_text(value, "SubComponent component_name")?)?
                }
                SUB_COMPONENT_VERSION => {
                    builder.version(value_to_num(value, "SubComponent version")?)?
                }
                SUB_COMPONENT_CODE_HASH => {
                    builder.code_hash(value_to_bytes(value, "SubComponent code_hash")?)?
                }
                SUB_COMPONENT_AUTHORITY_HASH => {
                    builder.authority_hash(value_to_bytes(value, "SubComponent authority_hash")?)?
                }
                k => {
                    error!("Unknown key in SubComponent: {}", k);
                    return Err(RequestProcessingError::InvalidDiceChain);
                }
            }
        }
        builder.build()
    }
}

#[derive(Debug, Clone, Default)]
struct SubComponentBuilder {
    name: OnceCell<String>,
    version: OnceCell<u64>,
    code_hash: OnceCell<Vec<u8>>,
    authority_hash: OnceCell<Vec<u8>>,
}

impl SubComponentBuilder {
    fn name(&mut self, name: String) -> Result<()> {
        set_once(&self.name, name, "SubComponent name")
    }

    fn version(&mut self, version: u64) -> Result<()> {
        set_once(&self.version, version, "SubComponent version")
    }

    fn code_hash(&mut self, code_hash: Vec<u8>) -> Result<()> {
        set_once(&self.code_hash, code_hash, "SubComponent code_hash")
    }

    fn authority_hash(&mut self, authority_hash: Vec<u8>) -> Result<()> {
        set_once(&self.authority_hash, authority_hash, "SubComponent authority_hash")
    }

    fn build(mut self) -> Result<SubComponent> {
        let name = take_value(&mut self.name, "SubComponent name")?;
        let version = take_value(&mut self.version, "SubComponent version")?;
        let code_hash = take_value(&mut self.code_hash, "SubComponent code_hash")?;
        let authority_hash = take_value(&mut self.authority_hash, "SubComponent authority_hash")?;
        Ok(SubComponent { name, version, code_hash, authority_hash })
    }
}

fn to_mode(value: Value) -> Result<DiceMode> {
    let mode = match value {
        // Mode is supposed to be encoded as a 1-byte bstr, but some implementations instead
        // encode it as an integer. Accept either. See b/273552826.
        // If Mode is omitted, it should be treated as if it was NotConfigured, according to
        // the Open Profile for DICE spec.
        Value::Bytes(bytes) => {
            if bytes.len() != 1 {
                error!("Bytes array with invalid length for mode: {:?}", bytes.len());
                return Err(RequestProcessingError::InvalidDiceChain);
            }
            bytes[0].into()
        }
        Value::Integer(i) => i,
        v => return Err(CoseError::UnexpectedItem(cbor_value_type(&v), "bstr or int").into()),
    };
    let mode = match mode {
        x if x == (DiceMode::kDiceModeNormal as i64).into() => DiceMode::kDiceModeNormal,
        x if x == (DiceMode::kDiceModeDebug as i64).into() => DiceMode::kDiceModeDebug,
        x if x == (DiceMode::kDiceModeMaintenance as i64).into() => DiceMode::kDiceModeMaintenance,
        // If Mode is invalid, it should be treated as if it was NotConfigured, according to
        // the Open Profile for DICE spec.
        _ => DiceMode::kDiceModeNotInitialized,
    };
    Ok(mode)
}

#[derive(Default, Debug, Clone)]
struct PayloadBuilder {
    subject_public_key: OnceCell<PublicKey>,
    mode: OnceCell<DiceMode>,
    code_hash: OnceCell<[u8; HASH_SIZE]>,
    authority_hash: OnceCell<[u8; HASH_SIZE]>,
    config_descriptor: OnceCell<ConfigDescriptor>,
}

fn set_once<T>(field: &OnceCell<T>, value: T, field_name: &str) -> Result<()> {
    field.set(value).map_err(|_| {
        error!("Field '{field_name}' is duplicated in the Payload");
        RequestProcessingError::InvalidDiceChain
    })
}

fn take_value<T>(field: &mut OnceCell<T>, field_name: &str) -> Result<T> {
    field.take().ok_or_else(|| {
        error!("Field '{field_name}' is missing in the Payload");
        RequestProcessingError::InvalidDiceChain
    })
}

impl PayloadBuilder {
    fn subject_public_key(&mut self, key: PublicKey) -> Result<()> {
        set_once(&self.subject_public_key, key, "subject_public_key")
    }

    fn mode(&mut self, mode: DiceMode) -> Result<()> {
        set_once(&self.mode, mode, "mode")
    }

    fn code_hash(&mut self, code_hash: [u8; HASH_SIZE]) -> Result<()> {
        set_once(&self.code_hash, code_hash, "code_hash")
    }

    fn authority_hash(&mut self, authority_hash: [u8; HASH_SIZE]) -> Result<()> {
        set_once(&self.authority_hash, authority_hash, "authority_hash")
    }

    fn config_descriptor(&mut self, config_descriptor: ConfigDescriptor) -> Result<()> {
        set_once(&self.config_descriptor, config_descriptor, "config_descriptor")
    }

    fn build(mut self) -> Result<DiceChainEntryPayload> {
        let subject_public_key = take_value(&mut self.subject_public_key, "subject_public_key")?;
        // If Mode is omitted, it should be treated as if it was NotConfigured, according to
        // the Open Profile for DICE spec.
        let mode = self.mode.take().unwrap_or(DiceMode::kDiceModeNotInitialized);
        let code_hash = take_value(&mut self.code_hash, "code_hash")?;
        let authority_hash = take_value(&mut self.authority_hash, "authority_hash")?;
        let config_descriptor = take_value(&mut self.config_descriptor, "config_descriptor")?;
        Ok(DiceChainEntryPayload {
            subject_public_key,
            mode,
            code_hash,
            authority_hash,
            config_descriptor,
        })
    }
}
