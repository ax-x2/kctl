use crate::{
    boot,
    config::Config,
    keys,
    package::{self, PendingInstall, Readiness},
    release::Release,
    system::*,
};
use std::{
    fs,
    path::{Path, PathBuf},
};

#[derive(Default)]
pub struct Options {
    hardware: bool,
    skip_signature_check: bool,
}

impl Options {
    pub fn parse(flags: &[&str]) -> Result<Self> {
        let mut options = Self::default();
        for flag in flags {
            let value = match *flag {
                "--hardware" => &mut options.hardware,
                "--skip-signature-check" => &mut options.skip_signature_check,
                _ => return Err(format!("unknown install option {flag}").into()),
            };
            if *value {
                return Err(format!("duplicate install option {flag}").into());
            }
            *value = true;
        }
        Ok(options)
    }
}

pub fn deps() -> Result {
    distro()?;
    let _host = host_lock()?;
    sudo("apt-get", &["update"])?;
    sudo(
        "apt-get",
        &[
            "--no-remove",
            "install",
            "build-essential",
            "bc",
            "bison",
            "flex",
            "libssl-dev",
            "libelf-dev",
            "libncurses-dev",
            "libdw-dev",
            "dwarves",
            "zstd",
            "git",
            "fakeroot",
            "dpkg-dev",
            "debhelper",
            "rsync",
            "cpio",
            "kmod",
            "curl",
            "jq",
            "gzip",
            "xz-utils",
            "gpg",
            "ca-certificates",
            "python3",
            "perl",
            "openssl",
            "mokutil",
            "initramfs-tools",
        ],
    )
}
fn distro() -> Result {
    let os = fs::read_to_string("/etc/os-release")?;
    if !os
        .lines()
        .filter_map(|l| l.strip_prefix("ID="))
        .any(|id| matches!(id.trim_matches('"'), "debian" | "ubuntu"))
    {
        return Err("package installation supports Debian and Ubuntu".into());
    }
    Ok(())
}
pub fn precheck(c: &Config) -> Result<String> {
    check_build_host(c, "xz", true)
}
fn check_build_host(c: &Config, decoder: &str, verify_signature: bool) -> Result<String> {
    distro()?;
    if out(&mut cmd("id", &["-u"]))? == "0" {
        return Err("run kctl as a normal user with sudo access".into());
    }
    boot::space(c, true)?;
    let running = current()?;
    boot::bootable(&running)?;
    boot::entry(&running)?;
    let search = std::env::var_os("PATH").unwrap_or_default();
    for tool in [
        "curl",
        "tar",
        decoder,
        "timeout",
        "make",
        "gcc",
        "flex",
        "bison",
        "bc",
        "fakeroot",
        "dpkg-buildpackage",
        "dpkg-deb",
        "rsync",
        "cpio",
        "pahole",
    ]
    .into_iter()
    .chain(verify_signature.then_some("gpg"))
    {
        if !std::env::split_paths(&search).any(|dir| dir.join(tool).is_file()) {
            return Err(format!("missing {tool}. Run kctl deps").into());
        }
    }
    if Path::new("/sys/firmware/efi").exists() {
        let state = out(&mut cmd("mokutil", &["--sb-state"]))?;
        if !state.lines().any(|s| s == "SecureBoot disabled") {
            return Err(
                "unsigned kernel installation requires verified disabled Secure Boot".into(),
            );
        }
    }
    let conf = format!("/boot/config-{running}");
    fs::File::open(&conf)?;
    let root = home()?;
    private_dir(&root)?;
    let gib = available(&root)? / (1024 * MIB);
    println!(
        "Build   {gib} GiB available, up to {} parallel jobs",
        c.jobs
    );
    if gib < 12 {
        return Err(
            "build workspace needs at least 12 GiB free (debug builds may need more)".into(),
        );
    }
    Ok(conf)
}
fn packages(dir: &Path, r: &str) -> Result<(PathBuf, PathBuf)> {
    let [image_name, headers_name] = package::names(r);
    let mut image = None;
    let mut headers = None;
    for item in fs::read_dir(dir)? {
        let p = item?.path();
        if p.extension().and_then(|e| e.to_str()) != Some("deb") {
            continue;
        }
        let name = out(&mut cmd("dpkg-deb", &["-f", path(&p)?, "Package"]))?;
        let slot = if name == image_name {
            &mut image
        } else if name == headers_name {
            &mut headers
        } else {
            continue;
        };
        if slot.replace(p).is_some() {
            return Err(format!("found multiple build packages named {name}").into());
        }
    }
    Ok((
        image.ok_or("missing image package")?,
        headers.ok_or("missing headers package")?,
    ))
}
fn fresh(r: &str) -> Result {
    match package::readiness(r)? {
        Readiness::Absent => Ok(()),
        Readiness::Ready => Err(format!("{r} is already installed").into()),
        Readiness::Incomplete(reason) => {
            Err(format!("{r} is incomplete: {reason}. Run kctl recover repair {r}").into())
        }
    }
}
fn archive_signature(rel: &Release, skip: bool) -> Result<Option<&str>> {
    if skip {
        return Ok(None);
    }
    match rel.signature.as_deref() {
        Some(signature) if signature.ends_with(".tar.sign") => Ok(Some(signature)),
        Some(_) => Err("expected a detached kernel.org archive signature".into()),
        None => Err("upstream supplies no verifiable detached signature for this release. Pass --skip-signature-check to skip verification for a single install".into()),
    }
}

pub fn install(c: &Config, rel: Release, options: Options) -> Result {
    PendingInstall::require_empty()?;
    let signature = archive_signature(&rel, options.skip_signature_check)?;
    let decoder = if rel.url.ends_with(".tar.xz") {
        "xz"
    } else if rel.url.ends_with(".tar.gz") {
        "gzip"
    } else {
        return Err("expected a .tar.xz or .tar.gz kernel.org archive".into());
    };
    if options.skip_signature_check {
        eprintln!(
            "warning: skipping upstream signature verification for {}",
            rel.version
        );
    }
    step(1, 6, "Check host, fallback and available space");
    let conf = check_build_host(c, decoder, signature.is_some())?;
    let running = current()?;
    let expected = rel.kernel_release()?;
    fresh(&expected)?;
    println!(
        "Target  {}\nSource  {}\nDefault {running}",
        rel.version, rel.url
    );
    step(
        2,
        6,
        if signature.is_some() {
            "Download and verify upstream source"
        } else {
            "Download upstream source"
        },
    );
    let work = Temp::new(&home()?.join("work"))?;
    let src = work.0.join("source");
    fs::create_dir(&src)?;
    let archive = prepare_archive(&work.0, &rel.url, decoder, signature)?;
    run(&mut cmd(
        "tar",
        &[
            "-xf",
            path(&archive)?,
            "--strip-components=1",
            "-C",
            path(&src)?,
        ],
    ))?;
    fs::remove_file(archive)?;
    step(3, 6, "Configure from the running kernel");
    fs::copy(conf, src.join(".config"))?;
    run(cmd(
        "scripts/config",
        &[
            "--set-str",
            "LOCALVERSION",
            "-kctl",
            "--disable",
            "LOCALVERSION_AUTO",
            "--set-str",
            "SYSTEM_TRUSTED_KEYS",
            "",
            "--set-str",
            "SYSTEM_REVOCATION_KEYS",
            "",
        ],
    )
    .current_dir(&src))?;
    run(cmd("make", &["olddefconfig"]).current_dir(&src))?;
    let r = out(cmd("make", &["-s", "kernelrelease"]).current_dir(&src))?;
    if r != expected {
        return Err(format!("source produced {r}, expected {expected}").into());
    }
    if options.hardware || c.hardware {
        crate::hardware::scan(&src)?
    }
    step(4, 6, "Build Debian image and headers");
    let log_path = home()?.join("build.log");
    let log = fs::File::create(&log_path)?;
    println!("Build output: {}", log_path.display());
    run(cmd(
        "make",
        &[
            &format!("-j{}", c.jobs),
            "bindeb-pkg",
            &format!("KDEB_PKGVERSION={}-1", rel.version.replace("-rc", "~rc")),
        ],
    )
    .current_dir(&src)
    .env("DEB_BUILD_PROFILES", "pkg.linux-upstream.nokerneldbg")
    .stdout(log.try_clone()?)
    .stderr(log))
    .map_err(|e| format!("{e}. See {}", log_path.display()))?;
    let (image, headers) = packages(&work.0, &r)?;
    step(5, 6, "Pin running kernel and install alongside it");
    let _host = host_lock()?;
    boot::space(c, true)?;
    fresh(&r)?;
    // Estimate boot payload from the built files plus the current initramfs and headroom.
    let initrd = fs::metadata(format!("/boot/initrd.img-{running}"))?.len();
    let size = out(cmd("make", &["-s", "image_name"]).current_dir(&src))?;
    let required = fs::metadata(src.join(size))?.len()
        + fs::metadata(src.join("System.map"))?.len()
        + initrd
        + 64 * MIB;
    if available(Path::new("/boot"))? < required {
        return Err(format!(
            "boot payload estimate requires {} MiB",
            required.div_ceil(MIB)
        )
        .into());
    }
    boot::set_default(&running)?;
    let pending = PendingInstall::publish(&r, &work.0, &image, &headers)?;
    let result = (|| -> Result {
        pending.apt_install(false)?;
        step(6, 6, "Verify packages, initramfs, modules and GRUB");
        if !Path::new(&format!("/boot/initrd.img-{r}")).try_exists()? {
            sudo("update-initramfs", &["-c", "-k", &r])?;
        }
        sudo("update-grub", &[])?;
        package::require_ready(&r)?;
        boot::verify_pinned(&running)
    })();
    result.map_err(|e| format!("{e}. Packages retained for {r}. Run kctl recover repair {r}"))?;
    pending.discard()?;
    boot::keep_reserve(c);
    println!("Installed {r}. Try it once with kctl boot once {r}.");
    Ok(())
}

fn prepare_archive(
    work: &Path,
    source: &str,
    decoder: &str,
    signature_url: Option<&str>,
) -> Result<PathBuf> {
    let compressed = work.join("source.compressed");
    let signature = work.join("source.tar.sign");
    let archive = work.join("source.tar");
    let verifier = if let Some(url) = signature_url {
        let verifier = keys::Verifier::load(work)?;
        crate::release::download(url, &signature, 30, 64 * 1024)?;
        Some(verifier)
    } else {
        None
    };
    crate::release::download(source, &compressed, 15 * 60, 512 * MIB)?;
    decode_archive(&compressed, &archive, decoder)?;
    fs::remove_file(compressed)?;
    if let Some(verifier) = verifier {
        verifier.verify(&signature, &archive)?;
    }
    Ok(archive)
}

fn decode_archive(compressed: &Path, archive: &Path, decoder: &str) -> Result {
    let mut command = cmd("timeout", &["--kill-after=5s", "5m", decoder]);
    if decoder == "xz" {
        command.arg("--memlimit-decompress=256MiB");
    }
    stream_stdout(
        command
            .args(["-dc", "--", path(compressed)?])
            .env_remove("XZ_OPT")
            .env_remove("XZ_DEFAULTS")
            .env_remove("GZIP"),
        fs::File::create_new(archive)?,
        4 * 1024 * MIB,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skips_verification_only_when_explicitly_requested() {
        let candidate = crate::release::resolve("7.4-rc3").unwrap();
        let stable = crate::release::resolve("6.18").unwrap();
        let defaults = Options::parse(&[]).unwrap();
        assert!(archive_signature(&candidate, defaults.skip_signature_check).is_err());
        assert!(
            archive_signature(&stable, defaults.skip_signature_check)
                .unwrap()
                .is_some()
        );
        for flags in [
            ["--hardware", "--skip-signature-check"],
            ["--skip-signature-check", "--hardware"],
        ] {
            let options = Options::parse(&flags).unwrap();
            assert!(options.hardware);
            assert!(
                archive_signature(&candidate, options.skip_signature_check)
                    .unwrap()
                    .is_none()
            );
            assert!(
                archive_signature(&stable, options.skip_signature_check)
                    .unwrap()
                    .is_none()
            );
        }
        assert!(Options::parse(&["--skip-signature-check", "--skip-signature-check"]).is_err());
        assert!(Options::parse(&["--unknown"]).is_err());
    }

    #[test]
    fn decodes_gzip_without_a_filename_suffix() {
        let work = Temp::new(&std::env::temp_dir()).unwrap();
        let payload = work.0.join("payload");
        let compressed = work.0.join("source.compressed");
        let decoded = work.0.join("source.tar");
        fs::write(&payload, b"archive fixture").unwrap();
        run(cmd("gzip", &["-c", "--", path(&payload).unwrap()])
            .stdout(fs::File::create(&compressed).unwrap()))
        .unwrap();
        decode_archive(&compressed, &decoded, "gzip").unwrap();
        assert_eq!(fs::read(decoded).unwrap(), b"archive fixture");
    }
}
