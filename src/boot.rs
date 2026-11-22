use crate::{config::Config, package, system::*};
use std::{
    fs,
    io::{ErrorKind, Read},
    os::unix::fs::MetadataExt,
    path::Path,
};
const GRUB: &str = "/boot/grub/grub.cfg";
const GRUB_ENV: &str = "/boot/grub/grubenv";
const RESERVE: &str = "/boot/.kctl-grub-reserve";
pub fn entries(cfg: &str) -> Vec<(String, String)> {
    let mut parent = None;
    let mut items = Vec::new();
    for line in cfg.lines() {
        let l = line.trim_start();
        if line == l {
            parent = None
        }
        let id = l.split('\'').rev().nth(1).unwrap_or("");
        if l.starts_with("submenu ") && id.starts_with("gnulinux-advanced-") && token(id) {
            parent = Some(id);
            continue;
        }
        if l.starts_with("menuentry ")
            && let Some((release, _)) = id
                .strip_prefix("gnulinux-")
                .and_then(|s| s.rsplit_once("-advanced-"))
            && token(release)
            && token(id)
        {
            items.push((
                release.into(),
                parent
                    .map(|p| format!("{p}>{id}"))
                    .unwrap_or_else(|| id.into()),
            ))
        }
    }
    items
}
fn config() -> Result<String> {
    const LIMIT: u64 = 8 * MIB;
    match fs::File::open(GRUB) {
        Ok(file) => {
            let mut cfg = String::new();
            file.take(LIMIT + 1).read_to_string(&mut cfg)?;
            if cfg.len() as u64 > LIMIT {
                return Err("GRUB configuration exceeds its size limit".into());
            }
            Ok(cfg)
        }
        Err(e) if e.kind() == ErrorKind::PermissionDenied => {
            out_limit(&mut cmd("sudo", &["--", "cat", GRUB]), LIMIT)
        }
        Err(e) => Err(e.into()),
    }
}
pub fn menu() -> Result<Vec<(String, String)>> {
    Ok(entries(&config()?))
}
pub fn entry(release: &str) -> Result<String> {
    Ok(versioned_entry(&menu()?, release)?.to_owned())
}
fn versioned_entry<'a>(menu: &'a [(String, String)], release: &str) -> Result<&'a str> {
    let mut ids = menu.iter().filter(|(r, _)| r == release);
    let id = ids
        .next()
        .ok_or("kernel has no version-specific GRUB entry")?
        .1
        .as_str();
    if ids.next().is_some() {
        return Err("ambiguous GRUB entry".into());
    }
    Ok(id)
}
fn header(cfg: &str) -> impl Iterator<Item = &str> {
    cfg.lines()
        .skip_while(|line| !line.starts_with("### BEGIN ") || !line.ends_with("/00_header ###"))
        .skip(1)
        .take_while(|line| !line.starts_with("### END "))
        .map(str::trim)
}
fn selects(cfg: &str, id: &str) -> bool {
    let expected = format!("set default=\"{id}\"");
    let mut defaults = header(cfg).filter(|line| {
        line.starts_with("set default=") && *line != "set default=\"${next_entry}\""
    });
    defaults.next() == Some(expected.as_str()) && defaults.next().is_none()
}
pub fn file_problem(r: &str) -> Result<Option<String>> {
    if !token(r) {
        return Err("invalid kernel release".into());
    }
    for p in [
        format!("/boot/vmlinuz-{r}"),
        format!("/boot/initrd.img-{r}"),
        format!("/lib/modules/{r}/modules.dep"),
    ] {
        match fs::metadata(&p) {
            Ok(meta) if meta.is_file() && meta.len() > 0 => {}
            Ok(_) => return Ok(Some(format!("{p} is not a nonempty regular file"))),
            Err(e) if e.kind() == ErrorKind::NotFound => {
                return Ok(Some(format!("missing {p}")));
            }
            Err(e) => return Err(e.into()),
        }
    }
    Ok(None)
}
pub fn bootable(r: &str) -> Result {
    match file_problem(r)? {
        None => Ok(()),
        Some(problem) => Err(problem.into()),
    }
}
fn default_selected(r: &str) -> Result<bool> {
    let cfg = config()?;
    let menu = entries(&cfg);
    let id = versioned_entry(&menu, r)?;
    Ok(selects(&cfg, id))
}
pub fn verify_default(r: &str) -> Result {
    if !default_selected(r)? {
        return Err(format!("generated GRUB configuration does not select {r} by default").into());
    }
    Ok(())
}
pub fn verify_pinned(r: &str) -> Result {
    verify_default(r)?;
    if next_entry(&environment()?)?.is_some() {
        return Err("a pending GRUB one-shot entry overrides the pinned default".into());
    }
    Ok(())
}
pub fn set_default(r: &str) -> Result {
    boot_mount()?;
    bootable(r)?;
    let id = entry(r)?;
    let temp = Temp::new(&home()?.join("work"))?;
    let p = temp.0.join("default.cfg");
    fs::write(
        &p,
        format!("GRUB_DEFAULT='{id}'\nGRUB_TOP_LEVEL='/boot/vmlinuz-{r}'\n"),
    )?;
    const DROPIN: &str = "/etc/default/grub.d/zzzz-kctl.cfg";
    const STAGED: &str = "/etc/default/grub.d/.kctl-default.tmp";
    let previous = match fs::read(DROPIN) {
        Ok(bytes) => Some(bytes),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e.into()),
    };
    let publish = || -> Result {
        sudo("install", &["-D", "-m", "0644", path(&p)?, STAGED])?;
        sudo("mv", &["-fT", "--", STAGED, DROPIN])
    };
    publish()?;
    let refresh = (|| -> Result {
        sudo("update-grub", &[])?;
        verify_default(r)?;
        clear_next_entry()
    })();
    if let Err(error) = refresh {
        let rollback = (|| -> Result {
            if let Some(bytes) = previous {
                fs::write(&p, bytes)?;
                publish()?
            } else {
                sudo("rm", &["--", DROPIN])?
            }
            sudo("update-grub", &[])
        })();
        if let Err(undo) = rollback {
            return Err(format!(
                "{error}; restoring the previous GRUB configuration failed: {undo}"
            )
            .into());
        }
        return Err(error);
    }
    Ok(())
}
fn probe(target: &str) -> Result<String> {
    out(&mut cmd(
        "sudo",
        &["--", "grub-probe", "--target", target, GRUB_ENV],
    ))
}
fn plain_boot_device() -> Result<bool> {
    let dev = out(&mut cmd(
        "findmnt",
        &["-enro", "SOURCE", "--target", GRUB_ENV],
    ))?;
    if !dev.starts_with("/dev/") || dev.contains(['\n', '[']) {
        return Ok(false);
    }
    let block = out(&mut cmd("lsblk", &["-dnpro", "TYPE,PKNAME", "--", &dev]))?;
    let mut cols = block.split_whitespace();
    match (cols.next(), cols.next(), cols.next()) {
        (Some("disk"), None, None) => Ok(true),
        (Some("part"), Some(parent), None) if parent.starts_with("/dev/") => {
            Ok(out(&mut cmd("lsblk", &["-dnro", "TYPE", "--", parent]))? == "disk")
        }
        _ => Ok(false),
    }
}
fn consumes_next_entry(cfg: &str) -> bool {
    [
        "load_env",
        "set default=\"${next_entry}\"",
        "set next_entry=",
        "save_env next_entry",
    ]
    .into_iter()
    .scan(header(cfg), |lines, expected| {
        Some(lines.any(|line| line == expected))
    })
    .all(|found| found)
}
fn environment() -> Result<String> {
    out(&mut cmd("sudo", &["--", "grub-editenv", GRUB_ENV, "list"]))
}
fn next_entry(env: &str) -> Result<Option<&str>> {
    let mut entries = env
        .lines()
        .filter_map(|line| line.strip_prefix("next_entry="));
    let first = entries.next();
    if entries.next().is_some() {
        return Err("GRUB environment contains multiple next_entry values".into());
    }
    Ok(first.filter(|entry| !entry.is_empty()))
}
fn clear_next_entry() -> Result {
    sudo("grub-editenv", &[GRUB_ENV, "unset", "next_entry"])?;
    if next_entry(&environment()?)?.is_some() {
        return Err("GRUB next_entry is still set".into());
    }
    Ok(())
}
pub fn once(r: &str) -> Result {
    boot_mount()?;
    bootable(r)?;
    let cfg = config()?;
    if !consumes_next_entry(&cfg)
        || probe("fs")? != "ext2"
        || !probe("abstraction")?.is_empty()
        || !plain_boot_device()?
    {
        return Err("cannot verify that GRUB can clear a one-shot boot on this layout. Select the kernel manually in GRUB".into());
    }
    let running = current()?;
    bootable(&running)?;
    let menu = entries(&cfg);
    let running_id = versioned_entry(&menu, &running)?;
    if !selects(&cfg, running_id) {
        return Err(
            format!("pin the running kernel first with kctl boot default {running}").into(),
        );
    }
    let id = versioned_entry(&menu, r)?;
    next_entry(&environment()?)?;
    let schedule = (|| -> Result {
        sudo("grub-reboot", &[id])?;
        if next_entry(&environment()?)? != Some(id) {
            return Err("GRUB did not record the requested one-shot entry".into());
        }
        Ok(())
    })();
    if let Err(error) = schedule {
        if let Err(cleanup) = clear_next_entry() {
            return Err(format!("{error}; clearing the one-shot entry failed: {cleanup}").into());
        }
        return Err(error);
    }
    Ok(())
}
pub fn space(c: &Config, required: bool) -> Result {
    boot_mount()?;
    let free = available(Path::new("/boot"))? / MIB;
    println!("/boot   {free} MiB available");
    if free < c.min_boot_mib {
        let warning = format!("low /boot space (threshold {} MiB)", c.min_boot_mib);
        if required {
            return Err(warning.into());
        }
        eprintln!("warning: {warning}")
    }
    Ok(())
}
fn grub_headroom() -> Result<u64> {
    let len = fs::metadata(GRUB)?.len();
    Ok(2 * len + MIB)
}
fn reserve() -> Result<Option<fs::Metadata>> {
    match fs::symlink_metadata(RESERVE) {
        Ok(m) if m.is_file() && m.uid() == 0 && m.nlink() == 1 => Ok(Some(m)),
        Ok(_) => Err(format!("unexpected file at {RESERVE}").into()),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}
fn remove_reserve() -> Result {
    if reserve()?.is_some() {
        sudo("rm", &["--", RESERVE])?;
    }
    Ok(())
}
fn allocate_reserve(needed: u64) -> Result {
    if reserve()?.is_none() {
        sudo("install", &["-m", "0600", "/dev/null", RESERVE])?;
    }
    sudo("fallocate", &["-l", &needed.to_string(), "--", RESERVE])?;
    if !reserve()?.is_some_and(|m| m.blocks() * 512 >= needed) {
        return Err("GRUB reserve was not fully allocated".into());
    }
    Ok(())
}
fn ext_free_bytes(stat: &str) -> Result<u64> {
    let mut fields = stat.split_whitespace();
    match (fields.next(), fields.next(), fields.next(), fields.next()) {
        (Some("ef53"), Some(block_size), Some(free), None) => block_size
            .parse::<u64>()?
            .checked_mul(free.parse()?)
            .ok_or("boot filesystem free-space count overflow".into()),
        (Some(_), Some(_), Some(_), None) => Ok(0),
        _ => Err("unexpected boot filesystem statistics".into()),
    }
}
fn verify_grub_headroom(needed: u64) -> Result {
    if available(Path::new("/boot"))? >= needed {
        return Ok(());
    }
    let stat = out(&mut cmd("stat", &["-f", "-c", "%t %S %f", "--", "/boot"]))?;
    if ext_free_bytes(&stat)? < needed {
        return Err(format!(
            "free at least {} KiB for GRUB on /boot before cleanup can pin the running kernel",
            needed.div_ceil(1024)
        )
        .into());
    }
    // Free ext filesystem blocks can include reserves unavailable even to root.
    // Verify that the privileged writer can allocate the estimated headroom.
    let probe = allocate_reserve(needed);
    remove_reserve()?;
    probe.map_err(|e| format!("could not allocate temporary space for GRUB on /boot: {e}").into())
}
fn provision_reserve(min_free: u64) -> Result {
    let needed = grub_headroom()?.max(4 * MIB);
    let existing = reserve()?;
    let allocated = existing.as_ref().map_or(0, |m| m.blocks() * 512);
    if allocated >= needed {
        return Ok(());
    }
    if available(Path::new("/boot"))? < min_free.saturating_add(needed) {
        return Err("insufficient space to reserve room for GRUB regeneration".into());
    }
    allocate_reserve(needed)?;
    sudo("sync", &["-f", "--", RESERVE])
}
pub fn keep_reserve(c: &Config) {
    if let Err(e) = provision_reserve(c.min_boot_mib * MIB) {
        eprintln!("warning: {e}");
    }
}
fn removable(
    r: &str,
    running: &str,
    entries: &[(String, String)],
    intact: impl Fn(&str) -> bool,
) -> bool {
    token(r)
        && r != running
        && entries.iter().any(|(v, _)| v == r)
        && entries
            .iter()
            .any(|(v, _)| v != r && v != running && intact(v))
}
pub fn clean(c: &Config, target: Option<&str>) -> Result {
    let _host = target.map(|_| host_lock()).transpose()?;
    let running = current()?;
    let installed = menu()?;
    let intact = |v: &str| versioned_entry(&installed, v).is_ok() && bootable(v).is_ok();
    let eligible = |r: &str| removable(r, &running, &installed, intact);
    let Some(r) = target else {
        for (release, _) in &installed {
            if eligible(release) {
                println!("{release}")
            }
        }
        println!(
            "Remove one with kctl clean RELEASE. The running kernel and one fallback are protected."
        );
        return Ok(());
    };
    if !eligible(r) {
        return Err("select a listed non-running release and keep one intact fallback".into());
    }
    boot_mount()?;
    bootable(&running)?;
    versioned_entry(&installed, &running)?;
    let fallback = installed
        .iter()
        .map(|(release, _)| release.as_str())
        .find(|v| *v != r && *v != running && intact(v))
        .ok_or("could not verify a fallback kernel")?;
    let image = format!("linux-image-{r}");
    let unsigned = format!("linux-image-unsigned-{r}");
    let headers = format!("linux-headers-{r}");
    let image_path = format!("/boot/vmlinuz-{r}");
    let owners = out(&mut cmd("dpkg-query", &["-S", &image_path]))?;
    let mut pkgs = Vec::new();
    for p in [&image, &unsigned] {
        if owners
            .lines()
            .any(|line| line.split_once(": ") == Some((p.as_str(), image_path.as_str())))
        {
            pkgs.push(p.as_str())
        }
    }
    if !pkgs.is_empty() && package::state(&headers)? == package::State::Installed {
        pkgs.push(headers.as_str())
    }
    if pkgs.is_empty() {
        return Err("no matching packages installed".into());
    }
    confirm(&format!(
        "Pin {running} as default and remove {}?",
        pkgs.join(", ")
    ))?;
    let already_pinned = default_selected(&running)? && next_entry(&environment()?)?.is_none();
    let headroom = grub_headroom()?;
    if available(Path::new("/boot"))? < headroom {
        remove_reserve()?;
    }
    if !already_pinned {
        verify_grub_headroom(headroom)?;
        set_default(&running)?;
    }
    // dpkg refuses unsatisfied reverse dependencies and never schedules additional removals.
    sudo("dpkg", &[&["--remove"][..], pkgs.as_slice()].concat())?;
    sudo("update-grub", &[])?;
    verify_pinned(&running)?;
    bootable(fallback)?;
    entry(fallback)?;
    keep_reserve(c);
    Ok(())
}
pub fn resize(action: &str, device: &str) -> Result {
    if !matches!(action, "grow" | "shrink") {
        return Err("expected grow or shrink".into());
    }
    let p = fs::canonicalize(device)?;
    let dev = path(&p)?;
    if !p.starts_with("/dev") || out(&mut cmd("lsblk", &["-dnro", "TYPE", dev]))? != "part" {
        return Err("select a plain boot partition by device path".into());
    }
    if !out(&mut cmd("lsblk", &["-nro", "MOUNTPOINTS", dev]))?.is_empty() {
        return Err("boot the live environment and unmount the target partition first".into());
    }
    let disk = out(&mut cmd("lsblk", &["-dnpo", "PKNAME", dev]))?;
    if !disk.starts_with("/dev/") {
        return Err("cannot identify parent disk".into());
    }
    println!(
        "In GParted select {dev}, choose Resize/Move, {action} its end boundary, review and apply."
    );
    confirm(&format!("Open {disk} for offline partition editing?"))?;
    run(&mut cmd("gparted", &[&disk]))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn grub_versioned_entries() {
        let cfg = "menuentry 'Ubuntu' $menuentry_id_option 'gnulinux-simple-uuid' {\n}\nsubmenu 'Advanced options' $menuentry_id_option 'gnulinux-advanced-uuid' {\n\tmenuentry 'Ubuntu, Linux 6.1' $menuentry_id_option 'gnulinux-6.1-advanced-uuid' {\n\t}\n\tmenuentry 'Recovery' $menuentry_id_option 'gnulinux-6.1-recovery-uuid' {\n\t}\n}\nmenuentry 'Flat' $menuentry_id_option 'gnulinux-6.2-advanced-uuid' {";
        assert_eq!(
            entries(cfg),
            [
                (
                    "6.1".into(),
                    "gnulinux-advanced-uuid>gnulinux-6.1-advanced-uuid".into()
                ),
                ("6.2".into(), "gnulinux-6.2-advanced-uuid".into())
            ]
        )
    }
}

#[cfg(test)]
mod cleanup_tests {
    use super::*;
    #[test]
    fn one_shot_requires_the_grub_header_and_a_persistent_fallback() {
        let cfg = "### BEGIN /etc/grub.d/00_header ###\n  load_env\n  set default=\"${next_entry}\"\n  set next_entry=\n  save_env next_entry\n  set default=\"running-id\"\n### END /etc/grub.d/00_header ###";
        assert!(consumes_next_entry(cfg));
        assert!(selects(cfg, "running-id"));
        assert!(!selects(cfg, "other-id"));
        assert!(!consumes_next_entry(
            &cfg.replace("save_env next_entry", "save_env saved_entry")
        ));
        assert!(!selects(
            &cfg.replace("set next_entry=", "set default=\"other-id\""),
            "running-id"
        ));
        assert_eq!(
            next_entry("next_entry=target\nother=x").unwrap(),
            Some("target")
        );
        assert_eq!(next_entry("next_entry=").unwrap(), None);
        assert!(next_entry("next_entry=target\nnext_entry=other").is_err());
    }
    #[test]
    fn cleanup_keeps_running_and_spare() {
        let menu: Vec<_> = ["old", "running", "spare"]
            .into_iter()
            .map(|s| (s.to_owned(), s.to_owned()))
            .collect();
        assert!(removable("old", "running", &menu, |_| true));
        assert!(!removable("running", "running", &menu, |_| true));
        assert!(!removable("old", "running", &menu, |v| v != "spare"));
        assert!(!removable("unknown", "running", &menu, |_| true));
    }
    #[test]
    fn counts_free_ext_blocks_without_assuming_other_filesystems_have_a_reserve() {
        assert_eq!(ext_free_bytes("ef53 4096 14080").unwrap(), 55 * MIB);
        assert_eq!(ext_free_bytes("ef53 4096 0").unwrap(), 0);
        assert_eq!(ext_free_bytes("9123683e 4096 14080").unwrap(), 0);
        assert!(ext_free_bytes("ef53 4096").is_err());
        assert!(ext_free_bytes("ef53 4096 18446744073709551615").is_err());
    }
}
