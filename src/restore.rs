use crate::{boot, config::Config, manage::names, system::*};
use std::{
    fs,
    io::{ErrorKind, Read},
    os::unix::fs::MetadataExt,
    path::Path,
};

const BOOT: &str = "/boot";
const JOURNAL: &str = "/boot/.kctl-restore";
const FINISHED: &str = "/boot/.kctl-restore-finished";
const PREPARED: &str = "prepared";
const COMMITTED: &str = "committed";

fn metadata(p: &Path) -> Result<Option<fs::Metadata>> {
    match fs::symlink_metadata(p) {
        Ok(m) => Ok(Some(m)),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

fn root_dir(p: &Path) -> Result<bool> {
    match metadata(p)? {
        None => Ok(false),
        Some(m) if m.is_dir() && m.uid() == 0 && m.mode() & 0o022 == 0 => Ok(true),
        Some(_) => Err(format!("invalid restore directory {}", p.display()).into()),
    }
}

fn root_file(p: &Path) -> Result<bool> {
    match metadata(p)? {
        None => Ok(false),
        Some(m) if m.is_file() && m.uid() == 0 && m.mode() & 0o022 == 0 => Ok(true),
        Some(_) => Err(format!("invalid restore journal file {}", p.display()).into()),
    }
}

pub fn check_unfinished() -> Result {
    if root_dir(Path::new(JOURNAL))? {
        return Err("a boot restore is pending. Run kctl recover for its recovery command".into());
    }
    Ok(())
}

pub fn pending_release() -> Result<Option<String>> {
    let journal = Path::new(JOURNAL);
    if !root_dir(journal)? {
        return Ok(None);
    }
    let manifest = journal.join("release");
    if !root_file(&manifest)? {
        if root_file(&journal.join(PREPARED))? {
            return Err("prepared restore journal is missing its manifest".into());
        }
        return Ok(Some(String::new()));
    }
    let record = read_small(&manifest, 256)?;
    let release = record
        .strip_prefix("kctl-restore-v1\n")
        .and_then(|record| record.strip_suffix('\n'))
        .filter(|r| token(r))
        .ok_or("invalid restore manifest")?;
    Ok(Some(release.to_owned()))
}

fn read_small(p: &Path, limit: u64) -> Result<String> {
    if !fs::symlink_metadata(p)?.is_file() {
        return Err(format!("{} must be a regular file", p.display()).into());
    }
    let mut s = String::new();
    fs::File::open(p)?.take(limit + 1).read_to_string(&mut s)?;
    if s.len() as u64 > limit {
        return Err(format!("{} exceeds its size limit", p.display()).into());
    }
    Ok(s)
}

fn hashes<'a>(text: &'a str, paths: &[&str]) -> Result<Vec<&'a str>> {
    let mut lines = text.lines();
    let hashes = paths
        .iter()
        .map(|name| {
            let (digest, file) = lines
                .next()
                .and_then(|line| line.split_once("  "))
                .ok_or("invalid SHA-256 record")?;
            if file != *name || digest.len() != 64 || !digest.bytes().all(|b| b.is_ascii_hexdigit())
            {
                return Err("invalid SHA-256 record");
            }
            Ok(digest)
        })
        .collect::<std::result::Result<Vec<_>, _>>()?;
    if lines.next().is_some() {
        return Err("unexpected entry in SHA-256 manifest".into());
    }
    Ok(hashes)
}

fn clean_finished() -> Result {
    if root_dir(Path::new(FINISHED))? {
        sudo("rm", &["-r", "--", FINISHED])?;
    }
    Ok(())
}

fn finish() -> Result {
    sudo("mv", &["-T", "--", JOURNAL, FINISHED])?;
    sudo("sync", &["-f", "--", BOOT])?;
    clean_finished()
}

fn replace_set(
    boot: &Path,
    journal: &Path,
    files: &[String; 4],
    source: &str,
    execute: impl Fn(&str, &[&str]) -> Result,
) -> Result {
    let boot_path = path(boot)?;
    let image = boot.join(&files[0]);
    execute("rm", &["-f", "--", path(&image)?])?;
    execute("sync", &["-f", "--", boot_path])?;

    let put = |name: &str| -> Result {
        let src = journal.join(source).join(name);
        let dest = boot.join(name);
        let Some(meta) = metadata(&src)? else {
            if source == "old" {
                return execute("rm", &["-f", "--", path(&dest)?]);
            }
            return Err(format!("missing staged file {}", src.display()).into());
        };
        if !meta.is_file() {
            return Err(format!("invalid staged file {}", src.display()).into());
        }
        if metadata(&dest)?.is_some_and(|m| m.dev() == meta.dev() && m.ino() == meta.ino()) {
            return Ok(());
        }
        let replacement = journal.join("replacement");
        execute("rm", &["-f", "--", path(&replacement)?])?;
        execute("ln", &["-T", "--", path(&src)?, path(&replacement)?])?;
        execute("mv", &["-fT", "--", path(&replacement)?, path(&dest)?])
    };

    for file in &files[1..] {
        put(file)?;
    }
    execute("sync", &["-f", "--", boot_path])?;
    put(&files[0])?;
    execute("sync", &["-f", "--", boot_path])
}

pub fn restore(c: &Config, r: &str) -> Result {
    let running = current()?;
    if !token(r) || r == running {
        return Err("boot a different kernel before restoring this release".into());
    }
    let dir = home()?.join("backups").join(r);
    let files = names(r);
    let manifest = read_small(&dir.join("SHA256SUMS"), 2048)?;
    let expected = hashes(&manifest, &files.each_ref().map(String::as_str))?;
    let _host = host_lock()?;
    boot::space(c, true)?;
    if !Path::new(&format!("/lib/modules/{r}/modules.dep")).is_file() {
        return Err("restore the matching modules package first".into());
    }
    let total = files.iter().try_fold(16 * MIB, |sum, f| -> Result<_> {
        let meta = fs::symlink_metadata(dir.join(f))?;
        if !meta.is_file() || meta.len() == 0 {
            return Err(format!("{f} in the backup must be a nonempty regular file").into());
        }
        sum.checked_add(meta.len())
            .ok_or("backup is too large".into())
    })?;
    if available(Path::new(BOOT))? < total {
        return Err("insufficient space for staged restore".into());
    }
    confirm(&format!(
        "Replace boot files for {r} from {}?",
        dir.display()
    ))?;
    clean_finished()?;
    boot::set_default(&running)?;
    let temp = Temp::new(&home()?.join("work"))?;
    let record = temp.0.join("release");
    fs::write(&record, format!("kctl-restore-v1\n{r}\n"))?;
    let journal = Path::new(JOURNAL);
    let boot_dir = Path::new(BOOT);
    sudo("mkdir", &["-m", "0755", "--", JOURNAL])?;
    let result = (|| -> Result {
        sudo(
            "install",
            &[
                "-m",
                "0644",
                "--",
                path(&record)?,
                path(&journal.join("release"))?,
            ],
        )?;
        let old = journal.join("old");
        let new = journal.join("new");
        sudo("mkdir", &["-m", "0711", "--", path(&old)?, path(&new)?])?;
        let mut staged = Vec::with_capacity(files.len());
        for file in &files {
            let existing = boot_dir.join(file);
            if let Some(meta) = metadata(&existing)? {
                if !meta.is_file() {
                    return Err(format!("{} is not a regular file", existing.display()).into());
                }
                sudo(
                    "ln",
                    &["-T", "--", path(&existing)?, path(&old.join(file))?],
                )?;
            }
            let dest = new.join(file);
            let mode = if file.starts_with("config-") {
                "0644"
            } else {
                "0600"
            };
            sudo(
                "install",
                &["-m", mode, "--", path(&dir.join(file))?, path(&dest)?],
            )?;
            staged.push(dest);
        }
        let paths = staged.iter().map(|p| path(p)).collect::<Result<Vec<_>>>()?;
        let sums = out(cmd("sudo", &["--", "sha256sum", "--"]).args(&paths))?;
        if hashes(&sums, &paths)? != expected {
            return Err("staged boot files do not match the backup checksums".into());
        }
        sudo("sync", &["-f", "--", BOOT])?;
        sudo(
            "install",
            &["-m", "0644", "/dev/null", path(&journal.join(PREPARED))?],
        )?;
        sudo("sync", &["-f", "--", BOOT])?;
        replace_set(boot_dir, journal, &files, "new", sudo)?;
        sudo("update-grub", &[])?;
        boot::bootable(r)?;
        boot::entry(r)?;
        boot::verify_pinned(&running)?;
        sudo(
            "install",
            &["-m", "0644", "/dev/null", path(&journal.join(COMMITTED))?],
        )?;
        sudo("sync", &["-f", "--", BOOT])?;
        finish()
    })();
    result.map_err(|e| format!("{e}. Run kctl recover restore {r}"))?;
    println!("Restored {r}");
    Ok(())
}

pub fn recover(r: &str) -> Result {
    if !token(r) {
        return Err("invalid kernel release".into());
    }
    let _host = host_lock_for_restore()?;
    boot_mount()?;
    let journal = Path::new(JOURNAL);
    let Some(release) = pending_release()? else {
        if root_dir(Path::new(FINISHED))? {
            clean_finished()?;
            println!("Removed completed restore journal files");
            return Ok(());
        }
        return Err("no restore journal found".into());
    };
    if !release.is_empty() && release != r {
        return Err(format!("restore journal is for {release}").into());
    }
    let prepared = root_file(&journal.join(PREPARED))?;
    let committed = root_file(&journal.join(COMMITTED))?;
    confirm(&format!("Recover the interrupted boot restore for {r}?"))?;
    clean_finished()?;
    if prepared && !committed {
        let running = current()?;
        if r == running {
            return Err("boot a different kernel before restoring this release".into());
        }
        boot::bootable(&running)?;
        boot::verify_pinned(&running)?;
        let boot = Path::new(BOOT);
        if !root_dir(&journal.join("old"))? {
            return Err("prepared journal is missing the original boot files directory".into());
        }
        replace_set(boot, journal, &names(r), "old", sudo)?;
        sudo("update-grub", &[])?;
        boot::verify_pinned(&running)?;
    }
    finish()?;
    println!("Recovered restore journal for {r}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interrupted_publication_withdraws_image_and_can_roll_back() {
        let temp = Temp::new(&std::env::temp_dir()).unwrap();
        let boot = temp.0.join("boot");
        let journal = boot.join("journal");
        fs::create_dir_all(journal.join("old")).unwrap();
        fs::create_dir(journal.join("new")).unwrap();
        let files = names("6.18.1-kctl");
        for file in &files {
            fs::write(journal.join("new").join(file), b"replacement").unwrap();
        }
        for file in &files[..2] {
            fs::write(boot.join(file), b"original").unwrap();
            fs::hard_link(boot.join(file), journal.join("old").join(file)).unwrap();
        }
        let image = boot.join(&files[0]);
        let fixture = |program: &str, args: &[&str]| run(&mut cmd(program, args));
        let interrupted = replace_set(&boot, &journal, &files, "new", |program, args| {
            if program == "mv" && args.last() == Some(&image.to_str().unwrap()) {
                return Err("interrupted before publishing image".into());
            }
            fixture(program, args)
        });
        assert!(interrupted.is_err());
        assert!(!image.exists());
        assert_eq!(fs::read(boot.join(&files[1])).unwrap(), b"replacement");
        for _ in 0..2 {
            replace_set(&boot, &journal, &files, "old", fixture).unwrap();
            for file in &files[..2] {
                assert_eq!(fs::read(boot.join(file)).unwrap(), b"original");
            }
            assert!(!boot.join(&files[2]).exists());
        }
    }
}
