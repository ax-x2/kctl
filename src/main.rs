mod boot;
mod config;
mod hardware;
mod install;
mod keys;
mod manage;
mod package;
mod release;
mod restore;
mod system;
use config::Config;
use std::{
    env,
    io::{self, IsTerminal, Write},
    path::Path,
};
use system::*;

const HELP: &str = "kctl  ·  Linux kernel manager for Debian and Ubuntu

Usage   kctl COMMAND [ARGS]       kctl shell
        kctl install latest-longterm|stable|stable-eol|mainline|VERSION

Inspect
  current                       Running kernel and /boot capacity
  kernels                       Upstream releases from kernel.org
  check [CHANNEL]               Compare upstream and running versions
  precheck                      Inspect host before a build
  hardware SOURCE               Scan source for detected PCI and USB IDs

Build
  deps                          Install distro build and discovery tools
  keys status                   Show pinned fingerprints and cached keys
  keys refresh                  Fetch and validate kernel.org signing keys
  install TARGET [--hardware]    Build and install alongside the running kernel
      --skip-signature-check       Skip source signature verification for this install
  update [CHANNEL]              Install if upstream is newer than running
  clean [RELEASE]               List or remove an old packaged kernel
  clean-work                    Remove interrupted downloads and builds

Boot and recovery
  boot list                     List selectable GRUB kernels
  boot once RELEASE             Select the kernel for the next boot
  boot default RELEASE          Set default and promote to top-level where supported
  boot resize grow|shrink DEV   Open GParted for an offline boot partition
  backup create [RELEASE]       Save boot files (defaults to running)
  backup list                   List local boot backups
  backup restore RELEASE        Restore boot files from another kernel
  recover                       Show recovery steps and available backups
  recover repair RELEASE        Resume or repair a kctl kernel installation
  recover restore RELEASE       Roll back an interrupted boot-file restore
  recover discard-install RELEASE  Delete retained build packages

Modules and settings
  modules list                  Show loaded modules
  modules available             List modules for the running kernel
  modules activate NAME         Load a module now
  modules setup NAME            Load now and at boot
  config init                   Create ~/.kctl/config.toml with defaults
  shell                         Interactive prompt (exit to leave)
  help                          Show this menu";

fn dispatch(a: &[&str]) -> Result {
    match a {
        ["help" | "-h" | "--help"] | [] => {
            println!("{HELP}");
            return Ok(());
        }
        ["-V" | "--version"] => {
            println!("kctl {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        ["config", "init"] => return config::init(),
        ["keys", "status"] => return keys::status(),
        ["keys", "refresh"] => return keys::refresh(),
        _ => {}
    }
    let c = Config::load()?;
    match a {
        ["current"] => {
            println!("Running {}", current()?);
            boot::space(&c, false)
        }
        ["kernels"] => {
            for r in release::catalog()? {
                println!(
                    "{:10} {:16} {}",
                    r.kind,
                    r.version,
                    if r.eol { "EOL" } else { "" }
                )
            }
            Ok(())
        }
        ["check" | "update"] | ["check" | "update", _] => {
            let ch = a.get(1).copied().unwrap_or(&c.channel);
            if !release::channel(ch) {
                return Err("expected an update channel".into());
            }
            let r = release::resolve(ch)?;
            let running = current()?;
            let rv = release::running_version(&running).ok_or("unknown running kernel version")?;
            let newer = release::version(&r.version).ok_or("invalid upstream version")? > rv;
            println!(
                "Running {running}   {ch} {}   {}",
                r.version,
                if newer {
                    "newer upstream release"
                } else {
                    "up to date"
                }
            );
            if a[0] == "update" && newer {
                let _guard = lock()?;
                let target = r.kernel_release()?;
                {
                    let _host = host_lock()?;
                    boot_mount()?;
                    match package::readiness(&target)? {
                        package::Readiness::Ready => {
                            println!("Already installed. Try kctl boot once {target}");
                            return Ok(());
                        }
                        package::Readiness::Incomplete(reason) => {
                            return Err(format!(
                                "{target} is incomplete: {reason}. Run kctl recover repair {target}"
                            )
                            .into());
                        }
                        package::Readiness::Absent => {}
                    }
                }
                install::install(&c, r, install::Options::default())?
            }
            Ok(())
        }
        ["precheck"] => {
            install::precheck(&c)?;
            println!("Host prechecks passed");
            Ok(())
        }
        ["hardware", src] => hardware::scan(Path::new(src)),
        ["install", target, flags @ ..] => {
            let options = install::Options::parse(flags)?;
            let _guard = lock()?;
            install::install(&c, release::resolve(target)?, options)
        }
        ["deps"] => {
            let _guard = lock()?;
            install::deps()
        }
        ["boot", "list"] => {
            for (r, id) in boot::menu()? {
                println!("{r}  {id}")
            }
            Ok(())
        }
        ["boot", "once", r] => {
            let _guard = host_lock()?;
            boot::once(r)
        }
        ["boot", "default", r] => {
            let _workspace = lock()?;
            let _guard = host_lock()?;
            boot::set_default(r)?;
            boot::keep_reserve(&c);
            Ok(())
        }
        ["boot", "resize", action, dev] => boot::resize(action, dev),
        ["clean"] | ["clean", _] => {
            let _guard = lock()?;
            boot::clean(&c, a.get(1).copied())
        }
        ["clean-work"] => {
            let _guard = lock()?;
            manage::clean_work()
        }
        ["backup", "list"] => manage::list(&home()?.join("backups")),
        ["backup", "create"] | ["backup", "create", _] => {
            let _guard = lock()?;
            let running = current()?;
            manage::backup(a.get(2).copied().unwrap_or(&running))
        }
        ["backup", "restore", r] => {
            let _guard = lock()?;
            restore::restore(&c, r)
        }
        ["recover"] => manage::recover(),
        ["recover", "repair", r] => {
            let _guard = lock()?;
            manage::repair(&c, r)
        }
        ["recover", "discard-install", r] => {
            let _guard = lock()?;
            manage::discard_install(r)
        }
        ["recover", "restore", r] => {
            let _guard = lock()?;
            restore::recover(r)
        }
        ["modules", "list"] => run(&mut cmd("lsmod", &[])),
        ["modules", "available"] => {
            let r = current()?;
            run(&mut cmd(
                "find",
                &[
                    &format!("/lib/modules/{r}/"),
                    "-type",
                    "f",
                    "-name",
                    "*.ko*",
                    "-printf",
                    "%f\\n",
                ],
            ))
        }
        ["modules", action @ ("activate" | "setup"), name] => {
            let _guard = lock()?;
            manage::module(action, name)
        }
        _ => Err("unknown command or arguments. Run kctl help".into()),
    }
}
fn shell() -> Result {
    println!("kctl   type help for commands, exit to leave");
    if let Err(e) = Config::load().and_then(|c| boot::space(&c, false)) {
        eprintln!("kctl: {e}")
    }
    let mut line = String::new();
    loop {
        print!("kctl › ");
        io::stdout().flush()?;
        line.clear();
        if io::stdin().read_line(&mut line)? == 0 {
            break;
        }
        let args: Vec<_> = line.split_whitespace().collect();
        match args.as_slice() {
            [] => continue,
            ["exit" | "quit"] => break,
            _ => {
                if let Err(e) = dispatch(&args) {
                    eprintln!("kctl: {e}")
                }
            }
        }
    }
    Ok(())
}
fn main() {
    let args: Vec<_> = env::args().skip(1).collect();
    let a: Vec<_> = args.iter().map(String::as_str).collect();
    let result = if a == ["shell"] || a.is_empty() && io::stdin().is_terminal() {
        shell()
    } else {
        dispatch(&a)
    };
    if let Err(e) = result {
        eprintln!("kctl: {e}");
        if matches!(
            a.first(),
            Some(&"install" | &"update" | &"boot" | &"backup" | &"clean")
        ) {
            eprintln!("Recovery options: kctl recover")
        }
        std::process::exit(1)
    }
}
