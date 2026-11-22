use std::{
    env,
    error::Error,
    fs::{self, File},
    io::{self, Read, Write},
    os::unix::fs::DirBuilderExt,
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Stdio},
};

pub type Result<T = ()> = std::result::Result<T, Box<dyn Error>>;
pub fn path(p: &Path) -> Result<&str> {
    Ok(p.to_str().ok_or("path must be UTF-8")?)
}
pub fn cmd(name: &str, args: &[&str]) -> Command {
    // Debian login shells can omit sbin for non-root users.
    let prog = if !name.contains('/') && Path::new("/usr/sbin").join(name).is_file() {
        Path::new("/usr/sbin").join(name)
    } else {
        PathBuf::from(name)
    };
    let mut c = Command::new(prog);
    c.env("LC_ALL", "C").args(args);
    c
}
pub fn run(c: &mut Command) -> Result {
    let name = c.get_program().to_string_lossy().into_owned();
    let s = c.status().map_err(|e| format!("{name}: {e}"))?;
    if !s.success() {
        return Err(format!("{name} exited with {s}").into());
    }
    Ok(())
}
pub fn out(c: &mut Command) -> Result<String> {
    out_limit(c, 64 * 1024)
}
pub fn out_limit(c: &mut Command, limit: u64) -> Result<String> {
    let mut bytes = Vec::new();
    stream_stdout(c, &mut bytes, limit)?;
    Ok(String::from_utf8(bytes)?.trim().to_owned())
}
pub fn stream_stdout(c: &mut Command, output: impl Write, limit: u64) -> Result {
    let status = capture_stdout(c, output, limit)?;
    if !status.success() {
        return Err(format!("{} exited with {status}", c.get_program().to_string_lossy()).into());
    }
    Ok(())
}
pub fn capture_stdout(c: &mut Command, mut output: impl Write, limit: u64) -> Result<ExitStatus> {
    let name = c.get_program().to_string_lossy().into_owned();
    let mut child = c
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| format!("{name}: {e}"))?;
    let result = (|| -> Result {
        let mut pipe = child.stdout.take().ok_or("missing stdout pipe")?;
        let copied = io::copy(&mut (&mut pipe).take(limit), &mut output)?;
        if copied == limit && pipe.read(&mut [0])? != 0 {
            return Err(format!("output exceeds {limit} bytes").into());
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = child.kill();
    }
    let status = child.wait()?;
    result.map_err(|e| format!("{name}: {e}"))?;
    Ok(status)
}
pub fn sudo(name: &str, args: &[&str]) -> Result {
    run(cmd("sudo", &["--", name]).args(args))
}
pub fn current() -> Result<String> {
    Ok(fs::read_to_string("/proc/sys/kernel/osrelease")?
        .trim()
        .into())
}
pub fn token(s: &str) -> bool {
    !s.is_empty()
        && s.len() < 200
        && s.as_bytes()[0].is_ascii_alphanumeric()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b".+_-".contains(&b))
}
pub fn home() -> Result<PathBuf> {
    Ok(PathBuf::from(env::var("HOME")?).join(".kctl"))
}
pub fn private_dir(p: &Path) -> Result {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(p)?;
    Ok(())
}
pub fn lock() -> Result<File> {
    let dir = home()?;
    private_dir(&dir)?;
    let f = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join("lock"))?;
    f.try_lock()
        .map_err(|e| format!("cannot lock kctl workspace: {e}"))?;
    Ok(f)
}
pub struct Temp(pub PathBuf);
impl Temp {
    pub fn new(parent: &Path) -> Result<Self> {
        private_dir(parent)?;
        let template = parent.join("work.XXXXXXXX");
        Ok(Self(
            out(&mut cmd("mktemp", &["-d", path(&template)?]))?.into(),
        ))
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        if self.0.exists()
            && let Err(e) = fs::remove_dir_all(&self.0)
        {
            eprintln!("cleanup {}: {e}", self.0.display())
        }
    }
}
pub fn confirm(action: &str) -> Result {
    use io::IsTerminal;
    if !io::stdin().is_terminal() {
        return Err("confirmation requires a terminal".into());
    }
    print!("{action} [y/N] ");
    io::stdout().flush()?;
    let mut s = String::new();
    io::stdin().read_line(&mut s)?;
    if s.trim() != "y" {
        return Err("cancelled".into());
    }
    Ok(())
}
pub fn step(i: usize, n: usize, label: &str) {
    println!(
        "\n[{}{}] {i}/{n}  {label}",
        "━".repeat(i),
        "·".repeat(n - i)
    )
}
pub fn available(p: &Path) -> Result<u64> {
    let s = out(&mut cmd("df", &["-B1", "--output=avail", "--", path(p)?]))?;
    Ok(s.lines()
        .last()
        .ok_or("missing df result")?
        .trim()
        .parse()?)
}
pub fn boot_mount() -> Result {
    let fstab = fs::read_to_string("/etc/fstab")?;
    for line in fstab.lines() {
        let mut cols = line.split('#').next().unwrap_or("").split_whitespace();
        let (Some(device), Some(target @ ("/boot" | "/boot/efi"))) = (cols.next(), cols.next())
        else {
            continue;
        };
        let actual = out(&mut cmd(
            "findmnt",
            &["-nro", "MAJ:MIN", "--mountpoint", target],
        ))
        .map_err(|_| format!("expected {device} mounted at {target}"))?;
        let source = if device.starts_with('/') {
            device.into()
        } else {
            out(&mut cmd("findfs", &[device]))?
        };
        let expected = out(&mut cmd("lsblk", &["-dnro", "MAJ:MIN", &source]))?;
        if actual != expected {
            return Err(format!("unexpected device mounted at {target}").into());
        }
    }
    Ok(())
}
/// Serialize the privileged phase across all kctl users on this host.
pub fn host_lock() -> Result<File> {
    let file = host_lock_for_restore()?;
    crate::restore::check_unfinished()?;
    Ok(file)
}
pub fn host_lock_for_restore() -> Result<File> {
    const LOCK: &str = "/run/kctl.lock";
    if !Path::new(LOCK).try_exists()? {
        sudo("touch", &[LOCK])?
    }
    let file = File::open(LOCK)?;
    file.try_lock()
        .map_err(|e| format!("cannot lock boot maintenance: {e}"))?;
    Ok(file)
}
pub const MIB: u64 = 1024 * 1024;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captures_the_exact_limit_without_buffering_excess() {
        assert_eq!(
            out_limit(&mut cmd("printf", &["%s", "four"]), 4).unwrap(),
            "four"
        );
        let mut bytes = Vec::new();
        let result = stream_stdout(&mut cmd("printf", &["%s", "oversize"]), &mut bytes, 4);
        assert!(result.is_err());
        assert_eq!(bytes, b"over");
    }
}
