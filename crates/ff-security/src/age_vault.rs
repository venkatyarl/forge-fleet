//! Age-based secret vault for fleet-wide secret distribution.
//!
//! Each fleet node generates an age keypair. The leader encrypts secrets
//! to each node's public key. Nodes decrypt with their private key.
//!
//! # Workflow
//! 1. `generate_keypair()` → store pubkey in Postgres, privkey locally (0600).
//! 2. `encrypt_to_recipients(secret, &[pubkey, ...])` → produces armored ciphertext.
//! 3. Copy ciphertext to target node (SSH, fleet_task, etc.).
//! 4. `decrypt(ciphertext, privkey)` → recover plaintext.

use age::secrecy::ExposeSecret;
use std::io::{Read, Write};

/// An age identity (private key) + recipient (public key) pair.
#[derive(Debug, Clone)]
pub struct AgeKeypair {
    pub identity: String,
    pub recipient: String,
}

/// Generate a new age keypair.
pub fn generate_keypair() -> AgeKeypair {
    let secret = age::x25519::Identity::generate();
    let recipient = secret.to_public().to_string();
    let identity = secret.to_string().expose_secret().to_string();
    AgeKeypair {
        identity,
        recipient,
    }
}

/// Encrypt plaintext to one or more age recipients.
///
/// Returns armored ASCII ciphertext suitable for copying over SSH or
/// embedding in fleet_tasks.
pub fn encrypt_to_recipients(plaintext: &str, recipients: &[String]) -> Result<String, String> {
    let recipient_objs: Vec<age::x25519::Recipient> = recipients
        .iter()
        .map(|r| {
            r.parse::<age::x25519::Recipient>()
                .map_err(|e| format!("invalid recipient: {e}"))
        })
        .collect::<Result<Vec<_>, _>>()?;

    let recipient_refs: Vec<&dyn age::Recipient> = recipient_objs
        .iter()
        .map(|r| r as &dyn age::Recipient)
        .collect();

    let mut encrypted = vec![];
    let mut writer = age::Encryptor::with_recipients(recipient_refs.into_iter())
        .map_err(|e| format!("encryptor: {e}"))?
        .wrap_output(&mut encrypted)
        .map_err(|e| format!("wrap output: {e}"))?;

    writer
        .write_all(plaintext.as_bytes())
        .map_err(|e| format!("write: {e}"))?;
    writer.finish().map_err(|e| format!("finish: {e}"))?;

    // Armor for easy transport.
    let mut armored = vec![];
    let mut armor_writer =
        age::armor::ArmoredWriter::wrap_output(&mut armored, age::armor::Format::AsciiArmor)
            .map_err(|e| format!("armor wrap: {e}"))?;
    armor_writer
        .write_all(&encrypted)
        .map_err(|e| format!("armor write: {e}"))?;
    armor_writer
        .finish()
        .map_err(|e| format!("armor finish: {e}"))?;

    String::from_utf8(armored).map_err(|e| format!("utf8: {e}"))
}

/// Decrypt armored ciphertext with an age identity (private key).
pub fn decrypt(ciphertext: &str, identity: &str) -> Result<String, String> {
    let ident = identity
        .parse::<age::x25519::Identity>()
        .map_err(|e| format!("invalid identity: {e}"))?;

    // De-armor.
    let mut dearmored = vec![];
    let mut armor_reader = age::armor::ArmoredReader::new(ciphertext.as_bytes());
    armor_reader
        .read_to_end(&mut dearmored)
        .map_err(|e| format!("dearmor: {e}"))?;

    let mut decrypted = vec![];
    let mut reader = age::Decryptor::new(&dearmored[..])
        .map_err(|e| format!("decryptor: {e}"))?
        .decrypt(std::iter::once(&ident as &dyn age::Identity))
        .map_err(|e| format!("decrypt: {e}"))?;
    reader
        .read_to_end(&mut decrypted)
        .map_err(|e| format!("read: {e}"))?;

    String::from_utf8(decrypted).map_err(|e| format!("utf8: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let kp = generate_keypair();
        let secret = "postgresql://user:test_pass_only@host/db";
        let encrypted = encrypt_to_recipients(secret, &[kp.recipient.clone()]).unwrap();
        let decrypted = decrypt(&encrypted, &kp.identity).unwrap();
        assert_eq!(decrypted, secret);
    }

    #[test]
    fn multi_recipient() {
        let alice = generate_keypair();
        let bob = generate_keypair();
        let secret = "multi-recipient test secret only";
        let encrypted =
            encrypt_to_recipients(secret, &[alice.recipient.clone(), bob.recipient.clone()])
                .unwrap();
        assert_eq!(decrypt(&encrypted, &alice.identity).unwrap(), secret);
        assert_eq!(decrypt(&encrypted, &bob.identity).unwrap(), secret);
    }
}
