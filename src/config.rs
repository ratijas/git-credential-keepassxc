use aes_gcm::aead::generic_array::{typenum, GenericArray};
#[cfg(feature = "encryption")]
use aes_gcm::aead::{Aead, NewAead};
#[cfg(feature = "encryption")]
use aes_gcm::Aes256Gcm;
use anyhow::{anyhow, Result};
#[cfg(feature = "encryption")]
use rand::distributions::Alphanumeric;
#[cfg(feature = "encryption")]
use rand::{thread_rng, Rng};
use serde::{de, Deserialize, Serialize};
use slog::*;
use std::cell::RefCell;
use std::fs;
use std::io::prelude::*;
use std::path::Path;
use std::str::FromStr;
#[cfg(feature = "yubikey")]
use yubico_manager::config as yubico_config;
#[cfg(feature = "yubikey")]
use yubico_manager::Yubico;

#[cfg(feature = "yubikey")]
const YUBIKEY_CHALLENGE_LENGTH: usize = 64usize;
#[cfg(feature = "yubikey")]
const YUBIKEY_RESPONSE_LENGTH: usize = 20usize;
#[cfg(feature = "encryption")]
const AES_KEY_LENGTH: usize = 32usize;
type AesKey = GenericArray<u8, typenum::U32>;
#[cfg(feature = "encryption")]
const AES_NONCE_LENGTH: usize = 12usize;
type AesNonce = GenericArray<u8, typenum::U12>;

#[derive(Serialize, Deserialize, Default, Debug)]
pub struct Config {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    databases: Vec<Database>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    encrypted_databases: Vec<EncryptedProfile>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    callers: Vec<Caller>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    encrypted_callers: Vec<EncryptedProfile>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub encryption: Vec<Encryption>,
}

impl Config {
    pub fn new() -> Self {
        Self {
            ..Default::default()
        }
    }

    pub fn read_from<T: AsRef<Path>>(config_path: T) -> Result<Self> {
        let json = fs::read_to_string(config_path.as_ref())?;
        let config: Config = serde_json::from_str(&json)?;
        Ok(config)
    }

    pub fn write_to<T: AsRef<Path>>(&self, config_path: T) -> Result<()> {
        let json = serde_json::to_string_pretty(self)?;
        let mut file = fs::File::create(config_path.as_ref())?;
        file.write_all(&json.as_bytes())?;
        Ok(())
    }

    pub fn get_databases(&self) -> Result<Vec<Database>> {
        let mut databases: Vec<_> = self.databases.clone();
        for encrypted_database in &self.encrypted_databases {
            let database_json =
                self.base64_decrypt(&encrypted_database.data, &encrypted_database.nonce)?;
            databases.push(serde_json::from_str(database_json.as_str())?);
        }
        Ok(databases)
    }

    pub fn count_databases(&self) -> usize {
        self.databases.len() + self.encrypted_databases.len()
    }

    pub fn count_encrypted_databases(&self) -> usize {
        self.encrypted_databases.len()
    }

    pub fn clear_databases(&mut self) {
        self.databases.clear();
        self.encrypted_databases.clear();
    }

    pub fn add_database(&mut self, database: Database, encrypted: bool) -> Result<()> {
        if encrypted {
            let (data, nonce) = self.base64_encrypt(&serde_json::to_string(&database)?)?;
            self.encrypted_databases
                .push(EncryptedProfile { data, nonce });
        } else {
            self.databases.push(database);
        }
        Ok(())
    }

    pub fn get_callers(&self) -> Result<Vec<Caller>> {
        let mut callers: Vec<_> = self.callers.clone();
        for encrypted_caller in &self.encrypted_callers {
            callers.push(serde_json::from_str(
                &self.base64_decrypt(&encrypted_caller.data, &encrypted_caller.nonce)?,
            )?);
        }
        Ok(callers)
    }

    pub fn count_callers(&self) -> usize {
        self.callers.len() + self.encrypted_callers.len()
    }

    pub fn count_encrypted_callers(&self) -> usize {
        self.encrypted_callers.len()
    }

    pub fn clear_callers(&mut self) {
        self.callers.clear();
        self.encrypted_callers.clear();
    }

    pub fn add_caller(&mut self, caller: Caller, encrypted: bool) -> Result<()> {
        if encrypted {
            let (data, nonce) = self.base64_encrypt(&serde_json::to_string(&caller)?)?;
            self.encrypted_callers
                .push(EncryptedProfile { data, nonce });
        } else {
            self.callers.push(caller);
        }
        Ok(())
    }

    #[cfg(not(feature = "encryption"))]
    fn base64_decrypt(&self, _data: &str, _nonce: &AesNonce) -> Result<String> {
        error!(
            crate::LOGGER.get().unwrap(),
            "Enable encryption to use this feature"
        );
        Err(anyhow!("Encryption is not enabled in this build"))
    }

    #[cfg(feature = "encryption")]
    fn base64_decrypt(&self, data: &str, nonce: &AesNonce) -> Result<String> {
        let key = self.get_encryption_key()?;
        let aead = Aes256Gcm::new(key.as_ref().unwrap());

        let decrypted = aead
            .decrypt(nonce, base64::decode(data)?.as_ref())
            .map_err(|_| anyhow!("Failed to decrypt database key"))?;
        Ok(String::from_utf8(decrypted)?)
    }

    #[cfg(not(feature = "encryption"))]
    fn base64_encrypt(&self, _data: &str) -> Result<(String, AesNonce)> {
        error!(
            crate::LOGGER.get().unwrap(),
            "Enable encryption to use this feature"
        );
        Err(anyhow!("Encryption is not enabled in this build"))
    }

    #[cfg(feature = "encryption")]
    fn base64_encrypt(&self, data: &str) -> Result<(String, AesNonce)> {
        let nonce = aes_nonce();
        let key = self.get_encryption_key()?;
        let aead = Aes256Gcm::new(key.as_ref().unwrap());

        let encrypted = aead
            .encrypt(&nonce, data.as_bytes())
            .map_err(|_| anyhow!("Failed to encrypt database key"))?;
        Ok((base64::encode(&encrypted), nonce))
    }

    #[cfg(feature = "encryption")]
    fn get_encryption_key(&self) -> Result<std::cell::Ref<Option<AesKey>>> {
        if self.encryption.is_empty() {
            return Err(anyhow!("No encryption profile found"));
        }
        let encryption = &self.encryption[0];
        match encryption {
            #[cfg(not(feature = "yubikey"))]
            Encryption::ChallengeResponse {
                serial: _,
                slot: _,
                challenge: _,
                response: _,
            } => {
                error!(
                    crate::LOGGER.get().unwrap(),
                    "Challenge-response encryption profile found however YubiKey is not enabled in this build"
                );
                Err(anyhow!("YubiKey is not enabled in this build"))
            }
            #[cfg(feature = "yubikey")]
            Encryption::ChallengeResponse {
                serial,
                slot,
                challenge,
                response,
            } => {
                if response.borrow().is_some() {
                    return Ok(response.borrow());
                }
                info!(
                    crate::LOGGER.get().unwrap(),
                    "Current challenge-response encryption profile was created using YubiKey {}",
                    serial.unwrap_or_default()
                );
                let mut yubi = Yubico::new();
                let device = yubi.find_yubikey()?;
                let config = yubico_config::Config::default()
                    .set_vendor_id(device.vendor_id)
                    .set_product_id(device.product_id);
                let curr_serial = yubi.read_serial_number(config).ok();
                if curr_serial.is_none() {
                    warn!(
                        crate::LOGGER.get().unwrap(),
                        "Failed to read YubiKey serial number"
                    );
                }
                debug!(
                    crate::LOGGER.get().unwrap(),
                    "Found YubiKey, vendor: {}, product: {}, serial: {}",
                    device.vendor_id,
                    device.product_id,
                    curr_serial.unwrap_or_default()
                );
                let slot = if *slot == 1 {
                    yubico_config::Slot::Slot1
                } else {
                    yubico_config::Slot::Slot2
                };
                debug!(crate::LOGGER.get().unwrap(), "Using YubiKey {:?}", slot);
                let config = yubico_config::Config::default()
                    .set_vendor_id(device.vendor_id)
                    .set_product_id(device.product_id)
                    .set_variable_size(true)
                    .set_mode(yubico_config::Mode::Sha1)
                    .set_slot(slot);
                debug!(crate::LOGGER.get().unwrap(), "Challenge: {}", challenge);
                info!(
                    crate::LOGGER.get().unwrap(),
                    "Retrieving response, tap your YubiKey if needed"
                );
                let hmac_result = yubi.challenge_response_hmac(challenge.as_bytes(), config)?;
                let mut hmac_response = vec![0u8; AES_KEY_LENGTH];
                hmac_response.splice(..YUBIKEY_RESPONSE_LENGTH, (*hmac_result).iter().cloned());
                *response.borrow_mut() = Some(AesKey::clone_from_slice(&hmac_response));
                Ok(response.borrow())
            }
        }
    }
}

#[cfg(feature = "encryption")]
fn aes_nonce() -> AesNonce {
    let mut rng = rand::thread_rng();
    let mut nonce = AesNonce::clone_from_slice(&[0u8; AES_NONCE_LENGTH]);
    rng.fill(nonce.as_mut_slice());
    nonce
}

fn aes_nonce_serialize<S>(nonce: &AesNonce, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    let nonce = base64::encode(nonce);
    serializer.serialize_str(&nonce)
}

fn aes_nonce_deserialize<'de, D>(deserializer: D) -> Result<AesNonce, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let nonce: &str = de::Deserialize::deserialize(deserializer)?;
    let nonce = base64::decode(nonce).map_err(|_| {
        de::Error::invalid_value(de::Unexpected::Str(nonce), &"base64 encoded data")
    })?;
    Ok(AesNonce::clone_from_slice(nonce.as_ref()))
}

#[derive(Serialize, Deserialize, Debug)]
struct EncryptedProfile {
    data: String,
    #[serde(
        serialize_with = "aes_nonce_serialize",
        deserialize_with = "aes_nonce_deserialize"
    )]
    nonce: AesNonce,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Database {
    pub id: String,
    pub key: String,
    pub pkey: String,
    pub group: String,
    pub group_uuid: String,
}

impl Database {
    pub fn new(
        id: String,
        id_seckey: crypto_box::SecretKey,
        group: crate::keepassxc::Group,
    ) -> Result<Self> {
        let id_seckey_b64 = base64::encode(id_seckey.to_bytes());
        let id_pubkey = id_seckey.public_key();
        let id_pubkey_b64 = base64::encode(id_pubkey.as_bytes());
        Ok(Self {
            id,
            key: id_seckey_b64,
            pkey: id_pubkey_b64,
            group: group.name,
            group_uuid: group.uuid,
        })
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Caller {
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uid: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gid: Option<u32>,
}

#[derive(Serialize, Deserialize, Debug)]
pub enum Encryption {
    ChallengeResponse {
        #[serde(skip_serializing_if = "Option::is_none")]
        serial: Option<u32>,
        slot: u8,
        challenge: String,
        #[serde(skip)]
        response: RefCell<Option<AesKey>>,
    },
}

impl FromStr for Encryption {
    type Err = anyhow::Error;

    fn from_str(profile: &str) -> Result<Self, Self::Err> {
        let profile_vec: Vec<_> = profile.split(':').collect();
        if profile_vec.is_empty() {
            return Err(anyhow!("Failed to parse encryption profile: {}", profile));
        }
        match profile_vec[0] {
            #[cfg(not(feature = "yubikey"))]
            "challenge-response" => {
                error!(
                    crate::LOGGER.get().unwrap(),
                    "YubiKey is not enabled in this build"
                );
                Err(anyhow!("YubiKey is not enabled in this build"))
            }
            #[cfg(feature = "yubikey")]
            "challenge-response" => {
                let mut yubi = Yubico::new();
                let device = yubi.find_yubikey()?;
                let config = yubico_config::Config::default()
                    .set_vendor_id(device.vendor_id)
                    .set_product_id(device.product_id);
                let serial = yubi.read_serial_number(config).ok();
                if serial.is_none() {
                    warn!(
                        crate::LOGGER.get().unwrap(),
                        "Failed to read YubiKey serial number"
                    );
                }
                let slot = if let Some(slot) = profile_vec.get(1) {
                    u8::from_str(slot)?
                } else {
                    2u8
                };
                if !(slot == 1 || slot == 2) {
                    return Err(anyhow!("Invalid YubiKey slot: {}", slot));
                }
                let rng = thread_rng();
                let challenge = if let Some(challenge) = profile_vec.get(2) {
                    (*challenge).to_owned()
                } else {
                    rng.sample_iter(Alphanumeric)
                        .take(YUBIKEY_CHALLENGE_LENGTH)
                        .collect()
                };
                Ok(Encryption::ChallengeResponse {
                    serial,
                    slot,
                    challenge,
                    response: RefCell::new(None),
                })
            }
            _ => Err(anyhow!("Unknown encryption profile: {}", profile)),
        }
    }
}
