use crate::{release, system::*};
use std::{
    fs::{self, File},
    io::{ErrorKind, Read},
    path::{Path, PathBuf},
    process::Command,
};

const CACHE: &str = "kernel-release-keyring.gpg";
const KEY_LIMIT: u64 = 256 * 1024;
const WKD: &str = "https://www.kernel.org/.well-known/openpgpkey/hu";

// Compare primary fingerprints against https://www.kernel.org/signature.html.
const SIGNERS: [Signer; 4] = [
    Signer {
        user: "torvalds",
        fingerprint: "ABAF11C65A2970B130ABE3C479BE3E4300411886",
        wkd_hash: "pf113mfnx1f3eb1yiwhsipa91xfc7o4x",
    },
    Signer {
        user: "gregkh",
        fingerprint: "647F28654894E3BD457199BE38DBBDC86092693E",
        wkd_hash: "e3n9xnm94c5apezqnj1pmrfuaoyfm8cf",
    },
    Signer {
        user: "sashal",
        fingerprint: "E27E5D8A3403A2EF66873BBCDEA66FF797772CDC",
        wkd_hash: "7j1cnb5wfkj3ts7cgy4xz1gwsz9xs5mj",
    },
    Signer {
        user: "bwh",
        fingerprint: "AC2B29BD34A6AFDDB3F68F35E7BFC8EC95861109",
        wkd_hash: "tjwaw1rrr3qfomkjoiah8fjsehyfe15r",
    },
];

struct Signer {
    user: &'static str,
    fingerprint: &'static str,
    wkd_hash: &'static str,
}

#[derive(PartialEq, Eq, Debug)]
struct Key<'a> {
    fingerprint: &'a str,
    validity: &'a str,
}

fn parse_keys(list: &str) -> Result<Vec<Key<'_>>> {
    let mut keys = Vec::new();
    let mut primary = None;
    for line in list.lines() {
        let mut fields = line.split(':');
        match fields.next() {
            Some("pub") if primary.is_none() => {
                primary = Some(fields.next().ok_or("missing public key validity")?);
            }
            Some("fpr") => {
                let Some(validity) = primary.take() else {
                    continue;
                };
                let fingerprint = fields.nth(8).ok_or("missing primary fingerprint")?;
                if !matches!(fingerprint.len(), 40 | 64)
                    || !fingerprint.bytes().all(|b| b.is_ascii_hexdigit())
                    || keys.iter().any(|key: &Key| key.fingerprint == fingerprint)
                {
                    return Err("invalid or duplicate primary fingerprint".into());
                }
                keys.push(Key {
                    fingerprint,
                    validity,
                });
            }
            Some("sec" | "ssb") => {
                return Err("signing key cache must contain public keys only".into());
            }
            _ if primary.is_some() => return Err("missing primary fingerprint".into()),
            _ => {}
        }
    }
    if primary.is_some() || keys.is_empty() {
        return Err("no complete primary key listing".into());
    }
    Ok(keys)
}

fn validate_keys<'a>(listing: &'a str, expected: &[&str]) -> Result<Vec<Key<'a>>> {
    let keys = parse_keys(listing)?;
    if keys.len() != expected.len() || !keys.iter().all(|key| expected.contains(&key.fingerprint)) {
        return Err("public key fingerprints differ from the pinned fingerprints".into());
    }
    Ok(keys)
}

fn gpg(work: &Path) -> Result<Command> {
    Ok(cmd(
        "gpg",
        &[
            "--no-options",
            "--homedir",
            path(work)?,
            "--batch",
            "--no-tty",
            "--no-autostart",
            "--no-auto-key-retrieve",
            "--no-auto-key-locate",
            "--quiet",
        ],
    ))
}

fn list_keys(work: &Path, file: &Path) -> Result<String> {
    out(gpg(work)?.args(["--with-colons", "--show-keys", "--", path(file)?]))
}

fn import_keys(work: &Path, file: &Path) -> Result {
    let mut log = Vec::new();
    let status = capture_stdout(
        gpg(work)?.args(["--logger-fd", "1", "--import", "--", path(file)?]),
        &mut log,
        64 * 1024,
    )?;
    if !status.success() {
        return Err(format!(
            "could not import signing keys ({status}): {}",
            String::from_utf8_lossy(&log).trim()
        )
        .into());
    }
    Ok(())
}

fn snapshot(cache: &Path, work: &Path) -> Result<Option<PathBuf>> {
    match fs::symlink_metadata(cache) {
        Ok(meta) if meta.is_file() && (1..=MIB).contains(&meta.len()) => {}
        Ok(_) => return Err("expected a regular signing key cache of at most 1 MiB".into()),
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let copy = work.join("cached.gpg");
    let bytes = std::io::copy(
        &mut File::open(cache)?.take(MIB + 1),
        &mut File::create_new(&copy)?,
    )?;
    if bytes == 0 || bytes > MIB {
        return Err("signing key cache exceeds its size limit or is empty".into());
    }
    Ok(Some(copy))
}

pub fn status() -> Result {
    let cache = home()?.join(CACHE);
    let work = Temp::new(&std::env::temp_dir())?;
    println!(
        "Kernel.org publishes these primary fingerprints at https://www.kernel.org/signature.html"
    );
    match snapshot(&cache, &work.0)? {
        Some(copy) => {
            let listing = list_keys(&work.0, &copy)?;
            let keys = validate_keys(&listing, &SIGNERS.each_ref().map(|s| s.fingerprint))
                .map_err(|e| format!("{e}. Run kctl keys refresh"))?;
            println!("Cache {}", cache.display());
            if let Ok(age) = fs::metadata(&cache)?.modified()?.elapsed() {
                println!("Cache age {} hours", age.as_secs() / 3600);
            }
            for signer in &SIGNERS {
                let key = keys
                    .iter()
                    .find(|key| key.fingerprint == signer.fingerprint)
                    .ok_or("cached signer is missing")?;
                let state = match key.validity {
                    "r" => "revoked",
                    "e" => "expired",
                    "d" => "disabled",
                    "i" => "invalid",
                    _ => "cached",
                };
                println!(
                    "{}@kernel.org  {}  {state}",
                    signer.user, signer.fingerprint
                );
            }
        }
        None => {
            for signer in &SIGNERS {
                println!(
                    "{}@kernel.org  {}  missing",
                    signer.user, signer.fingerprint
                );
            }
            println!("Run kctl keys refresh to populate the cache.");
        }
    }
    Ok(())
}

pub fn refresh() -> Result {
    let _guard = lock()?;
    let root = home()?;
    refresh_from(&root, |signer, dest| {
        let url = format!("{WKD}/{}?l={}", signer.wkd_hash, signer.user);
        release::download(&url, dest, 30, KEY_LIMIT)
    })?;
    println!(
        "Refreshed kernel.org signing keys at {}",
        root.join(CACHE).display()
    );
    Ok(())
}

fn refresh_from(root: &Path, fetch: impl Fn(&Signer, &Path) -> Result) -> Result {
    let cache = root.join(CACHE);
    let work = Temp::new(&root.join("work"))?;
    let key_home = work.0.canonicalize()?;
    if let Some(prior) = snapshot(&cache, &key_home)? {
        parse_keys(&list_keys(&key_home, &prior)?)?;
        import_keys(&key_home, &prior)?;
    }
    for signer in &SIGNERS {
        let file = key_home.join(signer.user);
        fetch(signer, &file).map_err(|e| format!("{}@kernel.org: {e}", signer.user))?;
        let listing = list_keys(&key_home, &file)?;
        validate_keys(&listing, &[signer.fingerprint]).map_err(|e| {
            format!(
                "{}@kernel.org: {e}; expected {}",
                signer.user, signer.fingerprint
            )
        })?;
        import_keys(&key_home, &file)?;
    }
    let staged = key_home.join("refreshed.gpg");
    let file = File::create_new(&staged)?;
    let expected = SIGNERS.each_ref().map(|s| s.fingerprint);
    stream_stdout(gpg(&key_home)?.arg("--export").args(expected), &file, MIB)?;
    validate_keys(&list_keys(&key_home, &staged)?, &expected)?;
    file.sync_all()?;
    File::open(&key_home)?.sync_all()?;
    fs::rename(&staged, cache)?;
    File::open(root)?.sync_all()?;
    Ok(())
}

pub struct Verifier {
    work: PathBuf,
    keyring: PathBuf,
}

impl Verifier {
    pub fn load(work: &Path) -> Result<Self> {
        let work = work.canonicalize()?;
        let keyring = snapshot(&home()?.join(CACHE), &work)?
            .ok_or("signing key cache is missing. Run kctl keys refresh")?;
        Self::from_snapshot(work, keyring)
    }

    fn from_snapshot(work: PathBuf, keyring: PathBuf) -> Result<Self> {
        let listing = list_keys(&work, &keyring)?;
        validate_keys(&listing, &SIGNERS.each_ref().map(|s| s.fingerprint))
            .map_err(|e| format!("{e}. Run kctl keys refresh"))?;
        Ok(Self { work, keyring })
    }

    pub fn verify(&self, signature: &Path, archive: &Path) -> Result {
        let output = out(gpg(&self.work)?.args([
            "--no-default-keyring",
            "--keyring",
            path(&self.keyring)?,
            "--trust-model",
            "always",
            "--status-fd",
            "1",
            "--verify",
            "--",
            path(signature)?,
            path(archive)?,
        ]))
        .map_err(|e| format!("kernel source signature verification failed: {e}"))?;
        verify_status(&output)
    }
}

fn verify_status(output: &str) -> Result {
    let mut good = 0;
    let mut valid = 0;
    for line in output.lines() {
        let Some(status) = line.strip_prefix("[GNUPG:] ") else {
            continue;
        };
        let mut fields = status.split_ascii_whitespace();
        match fields.next() {
            Some("GOODSIG") => good += 1,
            Some("VALIDSIG") => {
                let signing = fields.next().ok_or("missing signing key fingerprint")?;
                if fields.nth(7) != Some("00") {
                    return Err("expected a signature over the exact archive bytes".into());
                }
                let primary = fields.next().unwrap_or(signing);
                if !SIGNERS.iter().any(|signer| signer.fingerprint == primary) {
                    return Err("archive signature did not use a pinned kernel.org signer".into());
                }
                valid += 1;
            }
            Some(
                "BADSIG" | "ERRSIG" | "EXPSIG" | "EXPKEYSIG" | "REVKEYSIG" | "KEYREVOKED"
                | "KEYEXPIRED" | "SIGEXPIRED" | "NO_PUBKEY",
            ) => {
                return Err(
                    "archive signature or signing key is invalid, expired, or revoked".into(),
                );
            }
            _ => {}
        }
    }
    if good != 1 || valid != 1 {
        return Err("expected one valid archive signature from a pinned kernel.org signer".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_primary_fingerprints_instead_of_subkeys() {
        let fingerprint = SIGNERS[0].fingerprint;
        let other = SIGNERS[1].fingerprint;
        let listing = format!("pub:-:\nfpr:::::::::{fingerprint}:\nsub:-:\nfpr:::::::::{other}:");
        assert!(validate_keys(&listing, &[fingerprint]).is_ok());
        assert!(validate_keys(&listing, &[other]).is_err());
        let listing = format!("{listing}\npub:-:\nfpr:::::::::{other}:");
        assert!(validate_keys(&listing, &[fingerprint]).is_err());
        assert!(parse_keys("sec:-:").is_err());
    }

    #[test]
    fn requires_one_good_binary_signature_from_a_pinned_primary() {
        let primary = SIGNERS[0].fingerprint;
        let signing = "0000000000000000000000000000000000000001";
        let valid = format!(
            "[GNUPG:] GOODSIG signer\n[GNUPG:] VALIDSIG {signing} 2024-11-17 1731881708 0 4 0 1 10 00 {primary}"
        );
        assert!(verify_status(&valid).is_ok());
        assert!(verify_status(&valid.replace(primary, signing)).is_err());
        assert!(verify_status(&valid.replace("10 00", "10 01")).is_err());
        for failure in ["REVKEYSIG", "EXPKEYSIG", "EXPSIG", "KEYREVOKED", "BADSIG"] {
            assert!(verify_status(&valid.replace("GOODSIG", failure)).is_err());
        }
        assert!(verify_status(&format!("{valid}\n{valid}")).is_err());
    }
}
