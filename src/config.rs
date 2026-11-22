use crate::system::*;
use std::{
    fs,
    io::{ErrorKind, Write},
    thread,
};
const MAX_JOBS: usize = 64;
pub const TEMPLATE: &str = "# kctl settings\nchannel = \"latest-longterm\"\nmin_boot_mib = 512\nhardware_check = false\njobs = \"auto\"\n";
pub struct Config {
    pub channel: String,
    pub min_boot_mib: u64,
    pub jobs: usize,
    pub hardware: bool,
}
impl Config {
    pub fn parse(s: &str) -> Result<Self> {
        let mut c = Self {
            channel: "latest-longterm".into(),
            min_boot_mib: 512,
            jobs: thread::available_parallelism()
                .map(usize::from)
                .unwrap_or(1)
                .min(MAX_JOBS),
            hardware: false,
        };
        let mut seen = std::collections::HashSet::new();
        for line in s.lines() {
            let l = line.split('#').next().unwrap_or("").trim();
            if l.is_empty() {
                continue;
            }
            let (k, v) = l
                .split_once('=')
                .ok_or("expected key = value in config.toml")?;
            let (k, v) = (k.trim(), v.trim());
            if !seen.insert(k) {
                return Err(format!("duplicate setting {k}").into());
            }
            match k {
                "channel" => {
                    let ch = v
                        .strip_prefix('"')
                        .and_then(|v| v.strip_suffix('"'))
                        .or_else(|| v.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
                        .ok_or("quote the channel name")?;
                    if !crate::release::channel(ch) {
                        return Err("unknown update channel".into());
                    }
                    c.channel = ch.into()
                }
                "jobs" if matches!(v, "\"auto\"" | "'auto'") => {}
                "jobs" => match v.parse::<usize>() {
                    Ok(jobs @ 1..=MAX_JOBS) => c.jobs = jobs,
                    _ => {
                        return Err(format!(
                            "jobs must be \"auto\" or an integer from 1 to {MAX_JOBS}"
                        )
                        .into());
                    }
                },
                "min_boot_mib" => {
                    c.min_boot_mib = v.parse()?;
                    if !(1..=u64::MAX / MIB).contains(&c.min_boot_mib) {
                        return Err("invalid min_boot_mib".into());
                    }
                }
                "hardware_check" => c.hardware = v.parse()?,
                _ => return Err(format!("unknown setting {k}").into()),
            }
        }
        Ok(c)
    }
    pub fn load() -> Result<Self> {
        let file = home()?.join("config.toml");
        match fs::read_to_string(file) {
            Ok(s) => Self::parse(&s),
            Err(e) if e.kind() == ErrorKind::NotFound => Self::parse(""),
            Err(e) => Err(e.into()),
        }
    }
}
pub fn init() -> Result {
    let dir = home()?;
    private_dir(&dir)?;
    let p = dir.join("config.toml");
    fs::File::create_new(&p)?.write_all(TEMPLATE.as_bytes())?;
    println!("{}", p.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn settings() {
        let defaults = Config::parse(TEMPLATE).unwrap();
        assert_eq!(defaults.min_boot_mib, 512);
        let auto = Config::parse("").unwrap().jobs;
        assert_eq!(defaults.jobs, auto);
        assert_eq!(Config::parse("jobs = 'auto'").unwrap().jobs, auto);
        assert_eq!(
            Config::parse("channel = 'stable' # track stable\njobs = 2")
                .unwrap()
                .jobs,
            2
        );
        for s in [
            "jobs = 0",
            "jobs = 65",
            "jobs = auto",
            "jobs = \"invalid\"",
            "jobs = 1\njobs = 2",
            "min_boot_mib = 18446744073709551615",
            "channel = \"bogus\"",
        ] {
            assert!(Config::parse(s).is_err())
        }
    }
}
