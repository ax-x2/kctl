use crate::system::*;
use std::{fs::File, path::Path};
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version(u32, u32, u32, OptionKey);
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
enum OptionKey {
    Rc(u32),
    Final,
}
pub fn version(s: &str) -> Option<Version> {
    let (base, rc) = match s.split_once("-rc") {
        Some((v, n)) => (v, OptionKey::Rc(number(n)?)),
        None => (s, OptionKey::Final),
    };
    let mut p = base.split('.');
    let v = Version(
        number(p.next()?)?,
        number(p.next()?)?,
        p.next().map(number).unwrap_or(Some(0))?,
        rc,
    );
    p.next().is_none().then_some(v)
}
fn number(s: &str) -> Option<u32> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}
pub fn running_version(s: &str) -> Option<Version> {
    let base = s.split(['-', '+']).next()?;
    if let Some(rc) = s.strip_prefix(base)?.strip_prefix("-rc") {
        let n = rc.split(['-', '+']).next()?;
        return version(&format!("{base}-rc{n}"));
    }
    version(base)
}
pub fn channel(s: &str) -> bool {
    matches!(s, "latest-longterm" | "stable" | "stable-eol" | "mainline")
}
pub struct Release {
    pub kind: String,
    pub version: String,
    pub eol: bool,
    pub url: String,
    pub signature: Option<String>,
}
impl Release {
    pub fn kernel_release(&self) -> Result<String> {
        let Version(major, minor, patch, stage) =
            version(&self.version).ok_or("invalid upstream release")?;
        let base = format!("{major}.{minor}.{patch}");
        Ok(match stage {
            OptionKey::Final => format!("{base}-kctl"),
            OptionKey::Rc(n) => format!("{base}-rc{n}-kctl"),
        })
    }
}
pub fn catalog() -> Result<Vec<Release>> {
    const CATALOG_LIMIT: u64 = MIB;
    let work = Temp::new(&std::env::temp_dir())?;
    let json = work.0.join("releases.json");
    download(
        "https://www.kernel.org/releases.json",
        &json,
        30,
        CATALOG_LIMIT,
    )?;
    let rows = out_limit(
        &mut cmd(
            "jq",
            &[
                "-er",
                ".releases[] | [.moniker,.version,.iseol,.source,(.pgp // \"-\")] | @tsv",
                path(&json)?,
            ],
        ),
        CATALOG_LIMIT,
    )?;
    parse_rows(&rows)
}
pub fn download(url: &str, dest: &Path, seconds: u32, limit: u64) -> Result {
    if !["https://www.kernel.org/", "https://cdn.kernel.org/"]
        .iter()
        .any(|prefix| url.starts_with(prefix))
        || url
            .bytes()
            .any(|b| b.is_ascii_whitespace() || b.is_ascii_control())
    {
        return Err("download URL must use an approved kernel.org archive origin".into());
    }
    stream_stdout(
        &mut cmd(
            "curl",
            &[
                "-q",
                "-fsS",
                "--globoff",
                "--proto",
                "=https",
                "--connect-timeout",
                "20",
                "--max-time",
                &seconds.to_string(),
                "--",
                url,
            ],
        ),
        File::create_new(dest)?,
        limit,
    )
}
fn parse_rows(rows: &str) -> Result<Vec<Release>> {
    let mut list = Vec::new();
    for line in rows.lines() {
        let v: Vec<_> = line.split('\t').collect();
        if !matches!(v[0], "longterm" | "stable" | "mainline") {
            continue;
        }
        if v.len() != 5 {
            return Err("invalid release record".into());
        }
        if version(v[1]).is_none() {
            return Err("invalid release version".into());
        }
        if !v[3].starts_with("https://cdn.kernel.org/")
            && !v[3].starts_with("https://git.kernel.org/")
        {
            return Err("unexpected source URL in release feed".into());
        }
        list.push(Release {
            kind: v[0].into(),
            version: v[1].into(),
            eol: v[2].parse()?,
            url: v[3].into(),
            signature: (v[4] != "-").then(|| v[4].to_owned()),
        })
    }
    Ok(list)
}
pub fn select(rows: Vec<Release>, ch: &str) -> Result<Release> {
    let kind = match ch {
        "latest-longterm" => "longterm",
        "stable-eol" => "stable",
        s => s,
    };
    rows.into_iter()
        .filter(|r| r.kind == kind && r.eol == (ch == "stable-eol"))
        .max_by_key(|r| version(&r.version))
        .ok_or_else(|| format!("kernel.org currently lists no {ch} release").into())
}
pub fn resolve(s: &str) -> Result<Release> {
    if channel(s) {
        return select(catalog()?, s);
    }
    let Version(major, minor, patch, stage) =
        version(s).ok_or("expected an upstream version or release channel")?;
    let base = if patch == 0 {
        format!("{major}.{minor}")
    } else {
        format!("{major}.{minor}.{patch}")
    };
    let (version, url, signature) = match stage {
        OptionKey::Final => {
            let stem = format!("https://cdn.kernel.org/pub/linux/kernel/v{major}.x/linux-{base}");
            (
                base,
                format!("{stem}.tar.xz"),
                Some(format!("{stem}.tar.sign")),
            )
        }
        OptionKey::Rc(n) => {
            let rc = format!("{base}-rc{n}");
            let url = format!("https://git.kernel.org/torvalds/t/linux-{rc}.tar.gz");
            (rc, url, None)
        }
    };
    Ok(Release {
        kind: "version".into(),
        version,
        eol: false,
        url,
        signature,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    const FEED: &str = "stable\t6.9.9\tfalse\thttps://cdn.kernel.org/a\thttps://cdn.kernel.org/a.sign\nstable\t6.10.2\tfalse\thttps://cdn.kernel.org/b\thttps://cdn.kernel.org/b.sign\nstable\t6.8.1\ttrue\thttps://cdn.kernel.org/c\thttps://cdn.kernel.org/c.sign\nlinux-next\tnext-123\tfalse";
    #[test]
    fn channels_and_null_source() {
        assert_eq!(
            select(parse_rows(FEED).unwrap(), "stable").unwrap().version,
            "6.10.2"
        );
        assert_eq!(
            select(parse_rows(FEED).unwrap(), "stable-eol")
                .unwrap()
                .version,
            "6.8.1"
        );
        assert!(select(parse_rows(FEED).unwrap(), "mainline").is_err());
        assert!(resolve("7.2-rc1").unwrap().signature.is_none());
        assert!(
            resolve("6.12.0")
                .unwrap()
                .signature
                .unwrap()
                .ends_with("/linux-6.12.tar.sign")
        );
    }
    #[test]
    fn upstream_names() {
        assert_eq!(
            resolve("7.2-rc1").unwrap().kernel_release().unwrap(),
            "7.2.0-rc1-kctl"
        );
        assert!(running_version("7.2.0-rc1-kctl") < version("7.2"));
        assert_eq!(running_version("6.8.0-44-generic"), version("6.8"));
        for s in ["1", "../6.1", "6..1", "--help", "6.2.3.4"] {
            assert!(resolve(s).is_err())
        }
    }
}
