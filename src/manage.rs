use crate::{
    boot,
    config::Config,
    package::{self, PendingInstall, State},
    restore,
    system::*,
};
use std::{
    fs,
    io::ErrorKind,
    path::{Path, PathBuf},
};
const FILES: [&str; 4] = ["vmlinuz", "initrd.img", "config", "System.map"];
pub fn names(r: &str) -> [String; 4] {
    FILES.map(|p| format!("{p}-{r}"))
}
pub fn backup(r: &str) -> Result {
    if !token(r) {
        return Err("invalid kernel release".into());
    }
    let _host = host_lock()?;
    boot_mount()?;
    boot::bootable(r)?;
    let dir = home()?.join("backups");
    private_dir(&dir)?;
    let target = dir.join(r);
    if target.try_exists()? {
        return Err("backup already exists".into());
    }
    let tmp = Temp::new(&dir)?;
    for name in names(r) {
        let src = Path::new("/boot").join(&name);
        let dst = tmp.0.join(&name);
        match fs::copy(&src, &dst) {
            Ok(_) => {}
            Err(e) if e.kind() == ErrorKind::PermissionDenied => {
                run(cmd("sudo", &["cat", path(&src)?]).stdout(fs::File::create(dst)?))?
            }
            Err(e) => return Err(e.into()),
        }
    }
    let sums = out(cmd("sha256sum", &[]).current_dir(&tmp.0).args(names(r)))?;
    fs::write(tmp.0.join("SHA256SUMS"), format!("{sums}\n"))?;
    fs::rename(&tmp.0, &target)?;
    println!("Saved {}", target.display());
    Ok(())
}
pub fn list(dir: &Path) -> Result {
    let items = match fs::read_dir(dir) {
        Ok(i) => i,
        Err(e) if e.kind() == ErrorKind::NotFound => {
            println!("None");
            return Ok(());
        }
        Err(e) => return Err(e.into()),
    };
    for item in items {
        let item = item?;
        if item.file_type()?.is_dir() && item.path().join("SHA256SUMS").is_file() {
            println!("{}", item.file_name().to_string_lossy())
        }
    }
    Ok(())
}
/// Re-enter the package hooks after a partially configured install.
pub fn repair(c: &Config, r: &str) -> Result {
    let running = current()?;
    if !token(r) || !r.ends_with("-kctl") || r == running {
        return Err("repair requires a non-running kctl release".into());
    }
    let _host = host_lock()?;
    boot::space(c, true)?;
    let [image, headers] = package::names(r);
    let packages = [&headers, &image];
    let states = [package::state(&headers)?, package::state(&image)?];
    let pending = PendingInstall::load()?.filter(|p| p.release == r);
    let needs_packages = states
        .iter()
        .any(|state| matches!(state, State::Absent | State::Reinstall));
    let reinstall = if needs_packages {
        Some(pending.as_ref().ok_or_else(|| {
            format!("repair of {r} requires its matching image and headers .deb files")
        })?)
    } else {
        None
    };
    if let Some(build) = reinstall {
        build.verify()?;
    }
    confirm(&format!(
        "Pin {running} and rebuild the boot files for {r}?"
    ))?;
    boot::set_default(&running)?;
    if let Some(build) = reinstall {
        build.apt_install(true)?;
    } else {
        for (name, state) in packages.into_iter().zip(states) {
            if state == State::Configure {
                sudo("dpkg", &["--configure", name])?;
            }
        }
        if states.contains(&State::Triggers) {
            sudo("dpkg", &["--triggers-only", "--pending"])?;
        }
    }
    let initrd = format!("/boot/initrd.img-{r}");
    sudo(
        "update-initramfs",
        &[
            if Path::new(&initrd).try_exists()? {
                "-u"
            } else {
                "-c"
            },
            "-k",
            r,
        ],
    )?;
    sudo("update-grub", &[])?;
    package::require_ready(r)?;
    boot::verify_pinned(&running)?;
    if let Some(pending) = pending {
        pending.discard()?;
    }
    println!("Repaired {r}. Try it once with kctl boot once {r}.");
    Ok(())
}
pub fn discard_install(r: &str) -> Result {
    let pending = PendingInstall::load()?.ok_or("no retained build packages")?;
    if pending.release != r {
        return Err(format!("retained packages are for {}", pending.release).into());
    }
    confirm(&format!("Delete the retained build packages for {r}?"))?;
    pending.discard()?;
    println!("Discarded build packages for {r}. Resolve any partial dpkg installation separately.");
    Ok(())
}
pub fn recover() -> Result {
    println!("Boot the previous kernel under GRUB → Advanced options. From the working kernel:");
    println!(
        "  kctl boot default RELEASE    keep that kernel selected\n  kctl recover repair RELEASE  retry package hooks and rebuild initramfs\n  kctl backup list             find saved boot files\n  kctl backup restore RELEASE  restore another kernel's boot files\n  kctl boot once RELEASE       test the repaired kernel on the next boot"
    );
    println!("Running  {}", current()?);
    if let Some(release) = restore::pending_release()? {
        if release.is_empty() {
            println!("Restore preparation was interrupted. Run kctl recover restore RELEASE.");
        } else {
            println!("Restore is pending. Run kctl recover restore {release}.");
        }
    }
    if let Some(pending) = PendingInstall::load()? {
        println!(
            "Retained packages for {}. Run kctl recover repair {}.",
            pending.release, pending.release
        );
    }
    let dir = home()?.join("backups");
    println!("Backups");
    list(&dir)
}
pub fn module(action: &str, name: &str) -> Result {
    if !token(name)
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
    {
        return Err("invalid module name".into());
    }
    out(&mut cmd("modinfo", &["-F", "name", name]))?;
    sudo("modprobe", &[name])?;
    if action == "setup" {
        let tmp = Temp::new(&home()?.join("work"))?;
        let p = tmp.0.join("module.conf");
        fs::write(&p, format!("{name}\n"))?;
        sudo(
            "install",
            &[
                "-m",
                "0644",
                path(&p)?,
                &format!("/etc/modules-load.d/kctl-{name}.conf"),
            ],
        )?
    }
    Ok(())
}
pub fn clean_work() -> Result {
    let dir: PathBuf = home()?.join("work");
    if !dir.try_exists()? {
        return Ok(());
    }
    let mut dirs = Vec::new();
    for item in fs::read_dir(dir)? {
        let i = item?;
        let name = i.file_name();
        if i.file_type()?.is_dir()
            && name
                .to_str()
                .and_then(|s| s.strip_prefix("work."))
                .is_some_and(|s| s.len() == 8 && s.bytes().all(|b| b.is_ascii_alphanumeric()))
        {
            println!("{}", i.path().display());
            dirs.push(i.path())
        }
    }
    if !dirs.is_empty() {
        confirm("Delete these interrupted work directories?")?;
        for d in dirs {
            fs::remove_dir_all(d)?
        }
    }
    Ok(())
}
