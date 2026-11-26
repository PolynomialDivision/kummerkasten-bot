//! The server secret and what is derived from it: deletion codes (shown
//! once, stored only as an HMAC) and pseudonymous participation keys for
//! check-ins (M2).
//!
//! The master key is 32 random bytes in `store/kummerkasten.key` (mode
//! 0600), created on first start. Losing it makes existing deletion codes
//! useless; leaking it lets whoever also has the database test guesses:
//! keep it out of database backups' reach (see the README).

use std::path::Path;

use anyhow::{bail, Context, Result};
use hmac::{Hmac, Mac};
use rand::{rngs::OsRng, Rng, RngCore};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// Crockford base32: no I, L, O, U, nothing to misread.
const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
/// 16 characters × 5 bits = 80 bits of randomness, from the OS.
pub const TOKEN_CHARS: usize = 16;
/// Codes handed out before (12 characters, 60 bits) still work.
const LEGACY_TOKEN_CHARS: usize = 12;

#[derive(Clone)]
pub struct Secrets {
    token_key: [u8; 32],
    pseudonym_key: [u8; 32],
    idea_key: [u8; 32],
}

impl std::fmt::Debug for Secrets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secrets(…)")
    }
}

impl Secrets {
    /// Load the master key from `path`, creating it if it doesn't exist and
    /// `may_create`: not when the database already holds what was made with
    /// a key (a deploy that lost the file would otherwise let everyone
    /// answer a running check-in again and break every deletion code).
    pub fn load_or_create(path: &Path, may_create: bool) -> Result<Self> {
        let master = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && !may_create => {
                bail!(
                    "{} is missing, but the database holds data made with it (who answered \
                     which check-in, deletion codes). Restore the file (it belongs next to the \
                     database), or move the database away to start over.",
                    path.display()
                )
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let mut key = vec![0u8; 32];
                OsRng.fill_bytes(&mut key);
                write_private(path, &key)?;
                tracing::info!("Created a new Kummerkasten secret key");
                key
            }
            Err(e) => return Err(e).with_context(|| format!("Reading {}", path.display())),
        };
        if master.len() != 32 {
            bail!("{} must hold exactly 32 bytes", path.display());
        }
        Ok(Self::from_master(&master))
    }

    pub fn from_master(master: &[u8]) -> Self {
        Self {
            token_key: derive(master, b"kummerkasten/deletion-token/v1"),
            pseudonym_key: derive(master, b"kummerkasten/participation/v1"),
            idea_key: derive(master, b"kummerkasten/idea/v1"),
        }
    }

    /// What is stored for a deletion code (any spelling of it). Looking a
    /// code up compares these HMACs, never codes: without the key, timing
    /// can't help guess a code (and failed tries are rate-limited).
    pub fn token_hmac(&self, token: &str) -> Option<Vec<u8>> {
        let normalized = normalize_token(token)?;
        Some(hmac(&self.token_key, normalized.as_bytes()))
    }

    /// The key of `user`'s 👍 or 💬 on idea `idea_id`: one per account and
    /// idea, unlinkable to the user without the secret (and to their keys
    /// on other ideas or check-ins).
    pub fn idea_key(&self, idea_id: i64, user: &str) -> Vec<u8> {
        let mut msg = idea_id.to_be_bytes().to_vec();
        msg.push(0);
        msg.extend_from_slice(user.as_bytes());
        hmac(&self.idea_key, &msg)
    }

    /// The participation key of `user` in check-in run `run_id`: the same
    /// for the same pair, unlinkable to the user without the secret.
    pub fn participation_key(&self, run_id: i64, user: &str) -> Vec<u8> {
        let mut msg = run_id.to_be_bytes().to_vec();
        msg.push(0);
        msg.extend_from_slice(user.as_bytes());
        hmac(&self.pseudonym_key, &msg)
    }
}

fn derive(master: &[u8], label: &[u8]) -> [u8; 32] {
    let mut out = [0u8; 32];
    out.copy_from_slice(&hmac(master, label));
    out
}

fn hmac(key: &[u8], msg: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC takes any key length");
    mac.update(msg);
    mac.finalize().into_bytes().to_vec()
}

#[cfg(unix)]
fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::{io::Write, os::unix::fs::OpenOptionsExt};
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("Creating {}", path.display()))?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    std::fs::write(path, bytes).with_context(|| format!("Creating {}", path.display()))
}

/// A fresh deletion code, `XXXX-XXXX-XXXX-XXXX`.
pub fn new_token() -> String {
    let mut rng = OsRng;
    let chars: Vec<char> = (0..TOKEN_CHARS)
        .map(|_| ALPHABET[rng.gen_range(0..32)] as char)
        .collect();
    chars
        .chunks(4)
        .map(|group| group.iter().collect::<String>())
        .collect::<Vec<_>>()
        .join("-")
}

/// The canonical form of a typed code: dashes and spaces dropped, upper
/// case, and the letters Crockford reads as digits (O → 0, I/L → 1).
/// `None` unless it is a well-formed code (16 characters, or 12 for one
/// handed out before codes got longer).
pub fn normalize_token(input: &str) -> Option<String> {
    let mut out = String::with_capacity(TOKEN_CHARS);
    for c in input.chars() {
        let c = match c.to_ascii_uppercase() {
            '-' | ' ' | '\u{2010}'..='\u{2015}' => continue,
            'O' => '0',
            'I' | 'L' => '1',
            c => c,
        };
        if !ALPHABET.contains(&(c as u8)) || !c.is_ascii() {
            return None;
        }
        out.push(c);
    }
    (out.len() == TOKEN_CHARS || out.len() == LEGACY_TOKEN_CHARS).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_random_well_formed_and_forgiving_to_type() {
        let a = new_token();
        let b = new_token();
        assert_ne!(a, b);
        // XXXX-XXXX-XXXX-XXXX: 16 Crockford characters = 80 bits.
        assert_eq!(a.len(), 19);
        assert_eq!(a.split('-').map(str::len).collect::<Vec<_>>(), [4, 4, 4, 4]);
        const { assert!(TOKEN_CHARS * 5 >= 80) };
        assert!(normalize_token(&a).is_some());
        let secrets = Secrets::from_master(&[7; 32]);
        let hmac = secrets.token_hmac(&a).unwrap();
        // Lower case, spaces instead of dashes, O for 0: the same code.
        let sloppy = a.to_lowercase().replace('-', " ").replace('0', "o");
        assert_eq!(secrets.token_hmac(&sloppy).unwrap(), hmac);
        // Not stored as itself.
        assert!(!hmac
            .windows(TOKEN_CHARS)
            .any(|w| w == normalize_token(&a).unwrap().as_bytes()));
        // Garbage and wrong lengths are no codes.
        assert_eq!(normalize_token("hello"), None);
        assert_eq!(normalize_token("ABCD-EFGH-JKM"), None);
        assert_eq!(normalize_token("ABCD-EFGH-JKMU"), None);
        assert_eq!(normalize_token("ABCD-EFGH-JKMN-PQR"), None);
        // A code handed out before (12 characters) still reads.
        assert_eq!(
            normalize_token("abcd-efgh-jkmn").as_deref(),
            Some("ABCDEFGHJKMN")
        );
        // Every character position is random: no fixed prefix or pattern.
        let codes: Vec<String> = (0..200)
            .map(|_| normalize_token(&new_token()).unwrap())
            .collect();
        for i in 0..TOKEN_CHARS {
            let distinct: std::collections::HashSet<char> =
                codes.iter().map(|c| c.as_bytes()[i] as char).collect();
            assert!(distinct.len() > 10, "position {i}");
        }
    }

    #[test]
    fn hmacs_depend_on_the_secret() {
        let token = new_token();
        let one = Secrets::from_master(&[1; 32]);
        let two = Secrets::from_master(&[2; 32]);
        assert_ne!(one.token_hmac(&token), two.token_hmac(&token));
        assert_eq!(
            one.participation_key(4, "@a:x"),
            one.participation_key(4, "@a:x")
        );
        assert_ne!(
            one.participation_key(4, "@a:x"),
            one.participation_key(5, "@a:x")
        );
        assert_ne!(
            one.participation_key(4, "@a:x"),
            two.participation_key(4, "@a:x")
        );
    }

    #[test]
    fn the_key_file_is_created_once_and_private() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kummerkasten.key");
        let first = Secrets::load_or_create(&path, true).unwrap();
        let again = Secrets::load_or_create(&path, false).unwrap();
        let token = new_token();
        assert_eq!(first.token_hmac(&token), again.token_hmac(&token));
        // Lost while the database holds keyed data: refused, not replaced.
        let lost = dir.path().join("lost.key");
        let error = Secrets::load_or_create(&lost, false).unwrap_err();
        assert!(error.to_string().contains("is missing"), "{error}");
        assert!(!lost.exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }
}
