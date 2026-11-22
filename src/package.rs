use crate::{boot, system::*};
use std::{
    fs::{self, File},
    io::{ErrorKind, Read},
    path::{Path, PathBuf},
    process::Stdio,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum State {
    Absent,
    Reinstall,
    Configure,
    Triggers,
    Installed,
}

fn parse_state(status: &str) -> Result<State> {
    let invalid = || format!("unexpected dpkg status {status:?}");
    let (flag, state) = status.trim_end().split_once(' ').ok_or_else(invalid)?;
    let state = match state {
        "not-installed" | "config-files" => State::Absent,
        "half-installed" => State::Reinstall,
        "unpacked" | "half-configured" => State::Configure,
        "triggers-awaited" | "triggers-pending" => State::Triggers,
        "installed" => State::Installed,
        _ => return Err(invalid().into()),
    };
    match flag {
        "ok" => Ok(state),
        "reinstreq" => Ok(State::Reinstall),
        _ => Err(invalid().into()),
    }
}

pub fn state(name: &str) -> Result<State> {
    let mut output = Vec::new();
    let status = capture_stdout(
        cmd(
            "dpkg-query",
            &[
                "--no-pager",
                "-W",
                "-f=${db:Status-Eflag} ${db:Status-Status}\\n",
                name,
            ],
        )
        .env_remove("DPKG_ADMINDIR")
        .env_remove("DPKG_ROOT")
        .stderr(Stdio::null()),
        &mut output,
        1024,
    )?;
    match status.code() {
        Some(0) => parse_state(std::str::from_utf8(&output)?),
        Some(1) if output.is_empty() => Ok(State::Absent),
        _ => Err(format!("dpkg-query for {name} failed ({status})").into()),
    }
}

pub fn names(release: &str) -> [String; 2] {
    [
        format!("linux-image-{release}"),
        format!("linux-headers-{release}"),
    ]
}

#[derive(Debug, Eq, PartialEq)]
pub enum Readiness {
    Absent,
    Incomplete(String),
    Ready,
}

pub fn readiness(release: &str) -> Result<Readiness> {
    if !token(release) {
        return Err("invalid kernel release".into());
    }
    let [image, headers] = names(release);
    let states = [state(&image)?, state(&headers)?];
    let count = boot::menu()?.iter().filter(|(r, _)| r == release).count();
    if states == [State::Absent; 2] && count == 0 {
        for artifact in [
            format!("/boot/vmlinuz-{release}"),
            format!("/boot/initrd.img-{release}"),
            format!("/boot/config-{release}"),
            format!("/boot/System.map-{release}"),
            format!("/lib/modules/{release}"),
            format!("/usr/src/linux-headers-{release}"),
        ] {
            match fs::symlink_metadata(&artifact) {
                Ok(_) => return Ok(Readiness::Incomplete(format!("{artifact} already exists"))),
                Err(e) if e.kind() == ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        return Ok(Readiness::Absent);
    }
    if states != [State::Installed; 2] {
        return Ok(Readiness::Incomplete(format!(
            "package state for {image} is {:?} and for {headers} is {:?}",
            states[0], states[1]
        )));
    }
    if let Some(problem) = boot::file_problem(release)? {
        return Ok(Readiness::Incomplete(problem));
    }
    if count != 1 {
        return Ok(Readiness::Incomplete(format!(
            "expected one normal GRUB entry for {release}, found {count}"
        )));
    }
    Ok(Readiness::Ready)
}

pub fn require_ready(release: &str) -> Result {
    match readiness(release)? {
        Readiness::Ready => Ok(()),
        Readiness::Absent => Err(format!("kernel {release} is absent after package work").into()),
        Readiness::Incomplete(reason) => Err(format!(
            "kernel {release} is incomplete: {reason}. Run kctl recover repair {release}"
        )
        .into()),
    }
}

const FILES: [&str; 2] = ["image.deb", "headers.deb"];
const MANIFEST: &str = "manifest";
const SCHEMA: &str = "kctl-pending-install-v1";

pub struct PendingInstall {
    dir: PathBuf,
    pub release: String,
}

impl PendingInstall {
    pub fn load() -> Result<Option<Self>> {
        Self::load_from(&home()?)
    }

    fn load_from(root: &Path) -> Result<Option<Self>> {
        let dir = root.join("pending-install");
        match fs::symlink_metadata(&dir) {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => return Err("pending-install must be a directory".into()),
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        }
        let manifest = read_record(&dir.join(MANIFEST), 256)?;
        let mut lines = manifest.lines();
        let schema = lines.next();
        let release = lines
            .next()
            .filter(|r| token(r) && r.ends_with("-kctl"))
            .ok_or("invalid release in pending-install manifest")?;
        if schema != Some(SCHEMA) || lines.next().is_some() {
            return Err("invalid pending-install manifest".into());
        }
        Ok(Some(Self {
            dir,
            release: release.into(),
        }))
    }

    pub fn require_empty() -> Result {
        if let Some(pending) = Self::load()? {
            return Err(format!(
                "retained packages for {} exist. Run kctl recover repair {} or kctl recover discard-install {}",
                pending.release, pending.release, pending.release
            )
            .into());
        }
        Ok(())
    }

    pub fn publish(release: &str, work: &Path, image: &Path, headers: &Path) -> Result<Self> {
        Self::publish_at(&home()?, release, work, image, headers)
    }

    fn publish_at(
        root: &Path,
        release: &str,
        work: &Path,
        image: &Path,
        headers: &Path,
    ) -> Result<Self> {
        let stage = work.join("pending");
        fs::create_dir(&stage)?;
        for (src, name) in [image, headers].into_iter().zip(FILES) {
            if !fs::symlink_metadata(src)?.is_file() {
                return Err("built kernel package must be a regular file".into());
            }
            let file = stage.join(name);
            fs::rename(src, &file)?;
            File::open(file)?.sync_all()?;
        }
        let manifest = stage.join(MANIFEST);
        let checksums = stage.join("SHA256SUMS");
        fs::write(&manifest, format!("{SCHEMA}\n{release}\n"))?;
        fs::write(&checksums, format!("{}\n", sums(&stage)?))?;
        for file in [&manifest, &checksums, &stage] {
            File::open(file)?.sync_all()?;
        }
        let dir = root.join("pending-install");
        fs::rename(&stage, &dir)?;
        File::open(root)?.sync_all()?;
        File::open(work)?.sync_all()?;
        Ok(Self {
            dir,
            release: release.into(),
        })
    }

    pub fn apt_install(&self, reinstall: bool) -> Result {
        self.verify()?;
        let image = self.dir.join(FILES[0]);
        let headers = self.dir.join(FILES[1]);
        let mut args = vec!["--no-remove", "install", "-y"];
        if reinstall {
            args.push("--reinstall");
        }
        args.extend([path(&image)?, path(&headers)?]);
        sudo("apt-get", &args)
    }

    pub fn verify(&self) -> Result {
        for file in FILES {
            if !fs::symlink_metadata(self.dir.join(file))?.is_file() {
                return Err("retained kernel package must be a regular file".into());
            }
        }
        if sums(&self.dir)? != read_record(&self.dir.join("SHA256SUMS"), 256)?.trim_end() {
            return Err("retained kernel package checksum mismatch".into());
        }
        Ok(())
    }

    pub fn discard(self) -> Result {
        let root = self.dir.parent().ok_or("missing pending-install parent")?;
        let trash = Temp::new(&root.join("work"))?;
        fs::rename(&self.dir, trash.0.join("pending"))?;
        File::open(root)?.sync_all()?;
        File::open(&trash.0)?.sync_all()?;
        Ok(())
    }
}

fn sums(dir: &Path) -> Result<String> {
    out(cmd("sha256sum", &FILES).current_dir(dir))
}

fn read_record(path: &Path, limit: u64) -> Result<String> {
    if !fs::symlink_metadata(path)?.is_file() {
        return Err(format!("{} must be a regular file", path.display()).into());
    }
    let mut record = String::new();
    File::open(path)?
        .take(limit + 1)
        .read_to_string(&mut record)?;
    if record.len() as u64 > limit {
        return Err(format!("{} exceeds its size limit", path.display()).into());
    }
    Ok(record)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distinguishes_dpkg_failure_states() {
        assert_eq!(parse_state("ok installed\n").unwrap(), State::Installed);
        assert_eq!(parse_state("ok config-files").unwrap(), State::Absent);
        assert_eq!(parse_state("ok half-configured").unwrap(), State::Configure);
        assert_eq!(parse_state("ok triggers-pending").unwrap(), State::Triggers);
        assert_eq!(
            parse_state("reinstreq installed").unwrap(),
            State::Reinstall
        );
        assert!(parse_state("ok installed\nok installed").is_err());
    }

    #[test]
    fn published_packages_outlive_scratch_and_detect_changes() {
        let root = Temp::new(&std::env::temp_dir()).unwrap();
        let work = Temp::new(&root.0.join("work")).unwrap();
        let image = work.0.join("built-image.deb");
        let headers = work.0.join("built-headers.deb");
        fs::write(&image, b"image").unwrap();
        fs::write(&headers, b"headers").unwrap();
        PendingInstall::publish_at(&root.0, "6.18.1-kctl", &work.0, &image, &headers).unwrap();
        drop(work);
        let pending = PendingInstall::load_from(&root.0).unwrap().unwrap();
        assert_eq!(pending.release, "6.18.1-kctl");
        pending.verify().unwrap();
        fs::write(pending.dir.join(FILES[0]), b"changed").unwrap();
        assert!(pending.verify().is_err());
        pending.discard().unwrap();
        assert!(PendingInstall::load_from(&root.0).unwrap().is_none());
    }
}
