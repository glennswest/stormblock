//! The install-config's `storage:` section (#573, stormcos#902
//! STORAGE-LAYOUT.md): which drives an install takes, how each is formatted,
//! which carry a system leg and how the data set is built.
//!
//! The initramfs reads the section through `stormblock image storage-plan`:
//! the install-config is read for the `storage:` and `system_drive:` keys
//! only and never printed (it carries the pull secret and tokens, #275). The
//! plan comes back as shell assignments `/init` sources, or as one `STOP`
//! naming what was refused.
//!
//! The install-config is YAML. The engine carries no YAML crate (every
//! dependency costs every build), and the section is small and fixed, so a
//! subset is read here: block mappings and lists by indentation, flow `{}`
//! and `[]`, plain and quoted scalars, `#` comments.

use std::fmt::Write as _;

/// A YAML value of the subset read here.
#[derive(Debug, Clone, PartialEq)]
pub enum Yaml {
    Str(String),
    List(Vec<Yaml>),
    Map(Vec<(String, Yaml)>),
}

impl Yaml {
    fn as_str(&self) -> Option<&str> {
        match self {
            Yaml::Str(s) => Some(s),
            _ => None,
        }
    }
    fn kind(&self) -> &'static str {
        match self {
            Yaml::Str(_) => "a value",
            Yaml::List(_) => "a list",
            Yaml::Map(_) => "a mapping",
        }
    }
}

/// One line of a block: its indentation and its text, the comment gone.
#[derive(Debug, Clone)]
struct Line {
    indent: usize,
    text: String,
}

/// A line's text without its comment: `#` at the start or after a blank,
/// outside quotes.
fn strip_comment(s: &str) -> &str {
    let b = s.as_bytes();
    let mut q: Option<u8> = None;
    for i in 0..b.len() {
        let c = b[i];
        match q {
            Some(qc) if c == qc => q = None,
            Some(_) => {}
            None if c == b'"' || c == b'\'' => q = Some(c),
            None if c == b'#' && (i == 0 || b[i - 1] == b' ' || b[i - 1] == b'\t') => return &s[..i],
            None => {}
        }
    }
    s
}

fn lines_of(text: &str) -> Vec<Line> {
    text.lines()
        .filter_map(|l| {
            let l = strip_comment(l).trim_end();
            let t = l.trim_start();
            if t.is_empty() || t == "---" {
                return None;
            }
            Some(Line { indent: l.len() - t.len(), text: t.to_string() })
        })
        .collect()
}

/// `key: rest` → (key, rest), when the line is a mapping entry.
fn split_key(t: &str) -> Option<(String, String)> {
    let (key, rest) = if let Some(q) = t.chars().next().filter(|c| *c == '"' || *c == '\'') {
        let end = t[1..].find(q)? + 1;
        let after = &t[end + 1..];
        let rest = after.strip_prefix(':')?;
        (t[1..end].to_string(), rest)
    } else {
        let i = t.find(':')?;
        let key = &t[..i];
        if key.is_empty() || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.') {
            return None;
        }
        (key.to_string(), &t[i + 1..])
    };
    if !(rest.is_empty() || rest.starts_with(' ') || rest.starts_with('\t')) {
        return None;
    }
    Some((key, rest.trim().to_string()))
}

/// A flow value: `{a: 1, b: [x, y]}`, `[a, b]` or a scalar.
fn parse_flow(s: &str) -> Result<Yaml, String> {
    let chars: Vec<char> = s.trim().chars().collect();
    let mut i = 0;
    let v = flow_value(&chars, &mut i)?;
    skip_ws(&chars, &mut i);
    if i != chars.len() {
        return Err(format!("unexpected '{}' in '{}'", chars[i..].iter().collect::<String>(), s.trim()));
    }
    Ok(v)
}

fn skip_ws(c: &[char], i: &mut usize) {
    while *i < c.len() && c[*i].is_whitespace() {
        *i += 1;
    }
}

fn flow_scalar(c: &[char], i: &mut usize, stops: &[char]) -> Result<String, String> {
    skip_ws(c, i);
    if *i < c.len() && (c[*i] == '"' || c[*i] == '\'') {
        let q = c[*i];
        *i += 1;
        let start = *i;
        while *i < c.len() && c[*i] != q {
            *i += 1;
        }
        if *i >= c.len() {
            return Err("an unclosed quote".into());
        }
        let s: String = c[start..*i].iter().collect();
        *i += 1;
        return Ok(s);
    }
    let start = *i;
    while *i < c.len() && !stops.contains(&c[*i]) {
        *i += 1;
    }
    Ok(c[start..*i].iter().collect::<String>().trim().to_string())
}

fn flow_value(c: &[char], i: &mut usize) -> Result<Yaml, String> {
    skip_ws(c, i);
    match c.get(*i) {
        Some('[') => {
            *i += 1;
            let mut out = Vec::new();
            loop {
                skip_ws(c, i);
                match c.get(*i) {
                    Some(']') => {
                        *i += 1;
                        return Ok(Yaml::List(out));
                    }
                    None => return Err("an unclosed '['".into()),
                    _ => {}
                }
                out.push(flow_item(c, i, &[',', ']'])?);
                skip_ws(c, i);
                if c.get(*i) == Some(&',') {
                    *i += 1;
                }
            }
        }
        Some('{') => {
            *i += 1;
            let mut out = Vec::new();
            loop {
                skip_ws(c, i);
                match c.get(*i) {
                    Some('}') => {
                        *i += 1;
                        return Ok(Yaml::Map(out));
                    }
                    None => return Err("an unclosed '{'".into()),
                    _ => {}
                }
                let k = flow_scalar(c, i, &[':', ',', '}'])?;
                skip_ws(c, i);
                if c.get(*i) != Some(&':') {
                    return Err(format!("'{k}' in a mapping has no value"));
                }
                *i += 1;
                let v = flow_item(c, i, &[',', '}'])?;
                out.push((k, v));
                skip_ws(c, i);
                if c.get(*i) == Some(&',') {
                    *i += 1;
                }
            }
        }
        _ => Ok(Yaml::Str(flow_scalar(c, i, &[])?)),
    }
}

fn flow_item(c: &[char], i: &mut usize, stops: &[char]) -> Result<Yaml, String> {
    skip_ws(c, i);
    match c.get(*i) {
        Some('[') | Some('{') => flow_value(c, i),
        _ => Ok(Yaml::Str(flow_scalar(c, i, stops)?)),
    }
}

/// The block at `lines[*i..]` indented `indent`.
fn parse_block(lines: &mut Vec<Line>, i: &mut usize, indent: usize) -> Result<Yaml, String> {
    let is_item = |t: &str| t == "-" || t.starts_with("- ");
    if *i < lines.len() && is_item(&lines[*i].text) {
        let mut out = Vec::new();
        while *i < lines.len() && lines[*i].indent == indent && is_item(&lines[*i].text) {
            let rest = lines[*i].text[1..].trim_start().to_string();
            if rest.is_empty() {
                *i += 1;
                if *i < lines.len() && lines[*i].indent > indent {
                    let ind = lines[*i].indent;
                    out.push(parse_block(lines, i, ind)?);
                } else {
                    out.push(Yaml::Str(String::new()));
                }
            } else if split_key(&rest).is_some() {
                // `- key: v` then more keys below: a mapping item, its keys
                // at the column after the dash.
                let ind = indent + (lines[*i].text.len() - rest.len());
                lines[*i] = Line { indent: ind, text: rest };
                out.push(parse_block(lines, i, ind)?);
            } else {
                *i += 1;
                out.push(parse_flow(&rest)?);
            }
        }
        return Ok(Yaml::List(out));
    }
    let mut out = Vec::new();
    while *i < lines.len() && lines[*i].indent == indent {
        let Some((k, rest)) = split_key(&lines[*i].text) else {
            return Err(format!("'{}' is not 'key: value'", lines[*i].text));
        };
        *i += 1;
        let v = if !rest.is_empty() {
            parse_flow(&rest)?
        } else if *i < lines.len() && lines[*i].indent > indent {
            let ind = lines[*i].indent;
            parse_block(lines, i, ind)?
        } else if *i < lines.len() && lines[*i].indent == indent && is_item(&lines[*i].text) {
            parse_block(lines, i, indent)?
        } else {
            Yaml::Str(String::new())
        };
        if out.iter().any(|(ok, _): &(String, Yaml)| ok == &k) {
            return Err(format!("'{k}' is given twice"));
        }
        out.push((k, v));
    }
    Ok(Yaml::Map(out))
}

/// The top-level key `key` of a YAML document, read alone: None when the
/// document does not have it. Nothing else in the document is parsed.
pub fn top_level(doc: &str, key: &str) -> Result<Option<Yaml>, String> {
    let mut block: Option<Vec<Line>> = None;
    let mut inline: Option<String> = None;
    for raw in doc.lines() {
        let l = strip_comment(raw).trim_end();
        let t = l.trim_start();
        let top = !t.is_empty() && l.len() == t.len();
        if let Some(b) = block.as_mut() {
            if top {
                break;
            }
            if !t.is_empty() {
                b.push(Line { indent: l.len() - t.len(), text: t.to_string() });
            }
            continue;
        }
        if top {
            if let Some((k, rest)) = split_key(t) {
                if k == key {
                    if rest.is_empty() {
                        block = Some(Vec::new());
                    } else {
                        inline = Some(rest);
                        break;
                    }
                }
            }
        }
    }
    if let Some(rest) = inline {
        return parse_flow(&rest).map(Some);
    }
    let Some(mut lines) = block else { return Ok(None) };
    if lines.is_empty() {
        return Ok(Some(Yaml::Str(String::new())));
    }
    let ind = lines[0].indent;
    let mut i = 0;
    let v = parse_block(&mut lines, &mut i, ind)?;
    if i != lines.len() {
        return Err(format!("'{}' is not where it can be read (indentation)", lines[i].text));
    }
    Ok(Some(v))
}

/// Parse a whole small document (tests, and `lines_of`'s users).
pub fn parse(doc: &str) -> Result<Yaml, String> {
    let mut lines = lines_of(doc);
    if lines.is_empty() {
        return Ok(Yaml::Map(Vec::new()));
    }
    let ind = lines[0].indent;
    let mut i = 0;
    let v = parse_block(&mut lines, &mut i, ind)?;
    if i != lines.len() {
        return Err(format!("'{}' is not where it can be read (indentation)", lines[i].text));
    }
    Ok(v)
}

// ---------------------------------------------------------------------------
// The section

/// Which drives are taken.
#[derive(Debug, Clone, PartialEq)]
pub enum DriveSel {
    /// Every drive on the machine's own shelves (internal, front, rear).
    All,
    /// The drives named, by serial or bay (or a mapping of serial, wwn,
    /// shelf, bay): these are taken even on an external shelf, and even
    /// when they carry something.
    Listed(Vec<Vec<(String, String)>>),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Protection {
    None,
    /// Type 1, else none when the drive or its HBA refuses it (owner,
    /// 2026-11-13: "type1 if the HBA passes it, else none").
    Type1,
}

impl Protection {
    pub fn as_str(self) -> &'static str {
        match self {
            Protection::None => "none",
            Protection::Type1 => "type1",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Mirror {
    All,
    N(usize),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DataRaid {
    pub parity: u32,
    pub spares: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StorageSpec {
    pub drives: DriveSel,
    /// `linkRate` on a `storage.drives` entry (#642): (the entry's index,
    /// Gbit/s). The HBA phy facing that drive is held at it, at the owner's
    /// word, for a marginal link until the drive is replaced.
    pub link_rates: Vec<(usize, f32)>,
    pub block_size: u32,
    pub protection: Protection,
    /// auto | fast | full | none (stormdrive format's --method).
    pub method: String,
    pub system_mirror: Mirror,
    /// None: sized by the release.
    pub system_size: Option<u64>,
    /// None: `data_raid: none`, a data slab per drive's region, no set.
    pub data_raid: Option<DataRaid>,
    /// The wipe word for this install (#614): `wipe: all` (or `true`), or
    /// `wipe: {by: .., why: ..}` naming who asked and why. Every drive taken
    /// is formatted and laid whatever it holds, as with
    /// `rd.stormblock.assimilate=force`. None: what a drive holds is kept.
    pub wipe: Option<Wipe>,
    /// A drive that cannot be laid (#625): `stop` ends the install with
    /// why; `continue` lays the others without it when the plan still holds
    /// (fewer system legs, a spare in its place, the set one drive narrower).
    pub on_drive_failure: OnDriveFailure,
    /// `storage.runtime: true` (#623): a fresh install is done by the
    /// running node. `/init` boots the claimed image with every drive left
    /// alone and the engine formats, lays, makes the set and flows over
    /// (`mgmt::node_install`). A keep-data reinstall, and an install with the
    /// wipe word, are still laid by `/init` (option 1 on #623).
    pub runtime: bool,
}

/// `storage.on_drive_failure` (#625).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub enum OnDriveFailure {
    #[default]
    Stop,
    Continue,
}

impl OnDriveFailure {
    pub fn as_str(self) -> &'static str {
        match self {
            OnDriveFailure::Stop => "stop",
            OnDriveFailure::Continue => "continue",
        }
    }
}

/// Who asked for a wipe and why, as the install-config says (#614).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Wipe {
    pub by: String,
    pub why: String,
}

impl Wipe {
    /// Where the word came from, for the console.
    pub fn said(&self) -> String {
        let mut s = String::from("the install-config");
        match (self.by.is_empty(), self.why.is_empty()) {
            (true, true) => {}
            (false, true) => {
                let _ = write!(s, " ({})", self.by);
            }
            (true, false) => {
                let _ = write!(s, " ({})", self.why);
            }
            (false, false) => {
                let _ = write!(s, " ({}: {})", self.by, self.why);
            }
        }
        s
    }
}

impl Default for StorageSpec {
    fn default() -> Self {
        StorageSpec {
            drives: DriveSel::All,
            block_size: 4096,
            protection: Protection::Type1,
            method: "auto".into(),
            system_mirror: Mirror::All,
            system_size: None,
            data_raid: Some(DataRaid { parity: 1, spares: 0 }),
            wipe: None,
            on_drive_failure: OnDriveFailure::Stop,
            link_rates: Vec::new(),
            runtime: false,
        }
    }
}

fn keys_known(m: &[(String, Yaml)], what: &str, known: &[&str]) -> Result<(), String> {
    for (k, _) in m {
        if !known.contains(&k.as_str()) {
            return Err(format!("{what} has '{k}', which is none of: {}", known.join(", ")));
        }
    }
    Ok(())
}

fn scalar<'a>(v: &'a Yaml, what: &str) -> Result<&'a str, String> {
    v.as_str().ok_or_else(|| format!("{what} is {}, not a value", v.kind()))
}

fn number(v: &Yaml, what: &str) -> Result<u64, String> {
    let s = scalar(v, what)?;
    s.parse().map_err(|_| format!("{what} is '{s}', not a number"))
}

/// `128Gi`, `128G`, `137438953472`.
pub fn parse_bytes(s: &str) -> Result<u64, String> {
    let t = s.trim();
    let t = t.strip_suffix("iB").or_else(|| t.strip_suffix('i')).or_else(|| t.strip_suffix('B')).unwrap_or(t);
    let t = t.to_ascii_uppercase();
    crate::mgmt::config::parse_size(&t)
}

impl StorageSpec {
    /// The section as read by [`top_level`]. An empty `storage:` is the
    /// defaults.
    pub fn from_yaml(v: &Yaml) -> Result<StorageSpec, String> {
        let mut spec = StorageSpec::default();
        let m = match v {
            Yaml::Map(m) => m,
            Yaml::Str(s) if s.is_empty() => return Ok(spec),
            other => return Err(format!("storage is {}, not a mapping", other.kind())),
        };
        keys_known(m, "storage", &["drives", "format", "system_mirror", "system_size", "data_raid", "wipe", "on_drive_failure", "runtime"])?;
        for (k, v) in m {
            match k.as_str() {
                "drives" => {
                    spec.drives = match v {
                        Yaml::Str(s) if s == "all" || s.is_empty() => DriveSel::All,
                        Yaml::Str(s) => DriveSel::Listed(vec![vec![("any".into(), s.clone())]]),
                        Yaml::List(items) => {
                            let mut out = Vec::new();
                            for it in items {
                                match it {
                                    Yaml::Str(s) if !s.is_empty() => out.push(vec![("any".into(), s.clone())]),
                                    Yaml::Map(m) => {
                                        keys_known(m, "a storage.drives entry", &["serial", "wwn", "shelf", "bay", "linkRate"])?;
                                        let mut e = Vec::new();
                                        for (k, v) in m {
                                            let t = scalar(v, &format!("storage.drives' {k}"))?;
                                            if k == "linkRate" {
                                                let g: f32 = t.parse().map_err(|_| format!("storage.drives' linkRate is '{t}': 1.5, 3, 6, 12 or 22.5"))?;
                                                if ![1.5f32, 3.0, 6.0, 12.0, 22.5].contains(&g) {
                                                    return Err(format!("storage.drives' linkRate is '{t}': 1.5, 3, 6, 12 or 22.5"));
                                                }
                                                spec.link_rates.push((out.len(), g));
                                                continue;
                                            }
                                            e.push((k.clone(), t.to_string()));
                                        }
                                        if e.is_empty() {
                                            return Err("a storage.drives entry is empty".into());
                                        }
                                        out.push(e);
                                    }
                                    other => return Err(format!("a storage.drives entry is {}", other.kind())),
                                }
                            }
                            if out.is_empty() {
                                return Err("storage.drives is an empty list: name drives, or say all".into());
                            }
                            DriveSel::Listed(out)
                        }
                        other => return Err(format!("storage.drives is {}", other.kind())),
                    }
                }
                "format" => {
                    let Yaml::Map(f) = v else {
                        return Err(format!("storage.format is {}, not a mapping", v.kind()));
                    };
                    keys_known(f, "storage.format", &["block_size", "protection", "method"])?;
                    for (k, v) in f {
                        match k.as_str() {
                            "block_size" => {
                                let n = number(v, "storage.format.block_size")?;
                                if n != 512 && n != 4096 {
                                    return Err(format!("storage.format.block_size is {n}: 512 or 4096"));
                                }
                                spec.block_size = n as u32;
                            }
                            "protection" => {
                                spec.protection = match scalar(v, "storage.format.protection")? {
                                    "none" => Protection::None,
                                    "type1" => Protection::Type1,
                                    "type2" => {
                                        return Err("storage.format.protection is type2: stormdrive formats none or type1 \
                                                    (stormdrive#136); say type1 or none"
                                            .into())
                                    }
                                    o => return Err(format!("storage.format.protection is '{o}': none, type1 or type2")),
                                }
                            }
                            "method" => {
                                let s = scalar(v, "storage.format.method")?;
                                if !["auto", "fast", "full", "none"].contains(&s) {
                                    return Err(format!("storage.format.method is '{s}': auto, fast, full or none"));
                                }
                                spec.method = s.to_string();
                            }
                            _ => {}
                        }
                    }
                }
                "system_mirror" => {
                    let s = scalar(v, "storage.system_mirror")?;
                    spec.system_mirror = if s == "all" {
                        Mirror::All
                    } else {
                        let n: usize = s.parse().map_err(|_| format!("storage.system_mirror is '{s}': all or a number"))?;
                        if n == 0 {
                            return Err("storage.system_mirror is 0: a system needs a leg (1 or more, or all)".into());
                        }
                        Mirror::N(n)
                    };
                }
                "system_size" => {
                    let s = scalar(v, "storage.system_size")?;
                    spec.system_size = if s == "release" {
                        None
                    } else {
                        let n = parse_bytes(s).map_err(|e| format!("storage.system_size is '{s}': release or a size ({e})"))?;
                        if n < (1 << 30) {
                            return Err(format!("storage.system_size is '{s}': under 1 GiB"));
                        }
                        Some(n)
                    };
                }
                "data_raid" => {
                    spec.data_raid = match v {
                        Yaml::Str(s) if s == "none" => None,
                        Yaml::Str(s) if s.is_empty() => spec.data_raid,
                        Yaml::Map(r) => {
                            keys_known(r, "storage.data_raid", &["parity", "spares"])?;
                            let mut d = DataRaid { parity: 1, spares: 0 };
                            for (k, v) in r {
                                let n = number(v, &format!("storage.data_raid.{k}"))?;
                                match k.as_str() {
                                    "parity" if n <= 2 => d.parity = n as u32,
                                    "parity" => return Err(format!("storage.data_raid.parity is {n}: 0, 1 or 2")),
                                    _ => d.spares = n as u32,
                                }
                            }
                            Some(d)
                        }
                        other => return Err(format!("storage.data_raid is {}: a mapping or none", other.kind())),
                    };
                }
                "runtime" => {
                    let w = scalar(v, "storage.runtime")?;
                    spec.runtime = match w.to_ascii_lowercase().as_str() {
                        "true" | "yes" => true,
                        "false" | "no" | "" => false,
                        _ => return Err(format!("storage.runtime is '{w}': true or false")),
                    };
                }
                "on_drive_failure" => {
                    let w = scalar(v, "storage.on_drive_failure")?;
                    spec.on_drive_failure = match w.to_ascii_lowercase().as_str() {
                        "stop" | "" => OnDriveFailure::Stop,
                        "continue" => OnDriveFailure::Continue,
                        _ => return Err(format!("storage.on_drive_failure is '{w}': continue or stop")),
                    };
                }
                "wipe" => {
                    spec.wipe = match v {
                        Yaml::Str(s) => match s.to_ascii_lowercase().as_str() {
                            "all" | "true" | "yes" => Some(Wipe::default()),
                            "" | "false" | "no" | "none" => None,
                            _ => {
                                return Err(format!(
                                    "storage.wipe is '{s}': all (or true) to format every drive taken whatever it \
                                     holds, or leave it out"
                                ))
                            }
                        },
                        Yaml::Map(w) => {
                            keys_known(w, "storage.wipe", &["drives", "by", "why"])?;
                            let mut out = Wipe::default();
                            for (k, v) in w {
                                let t = scalar(v, &format!("storage.wipe.{k}"))?;
                                match k.as_str() {
                                    "drives" if t == "all" || t == "true" => {}
                                    "drives" => return Err(format!("storage.wipe.drives is '{t}': all")),
                                    "by" => out.by = t.to_string(),
                                    _ => out.why = t.to_string(),
                                }
                            }
                            Some(out)
                        }
                        other => return Err(format!("storage.wipe is {}: all, or a mapping (by, why)", other.kind())),
                    };
                }
                _ => {}
            }
        }
        Ok(spec)
    }
}

// ---------------------------------------------------------------------------
// The plan

/// A drive as the initramfs saw it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SeenDrive {
    pub dev: String,
    pub serial: String,
    pub wwn: String,
    pub model: String,
    pub shelf: String,
    /// internal | front | rear | external
    pub position: String,
    pub bay: String,
    pub bytes: u64,
    pub removable: bool,
    pub usb: bool,
    /// What it carries, when not blank and not this node's own layout: a
    /// stormblock install, a stormraid member, partitions, data.
    pub carries: String,
    /// The drive this boot runs from.
    pub running: bool,
    /// It answers and has no readable format (NOT READY, medium format
    /// corrupted, no capacity): no size until it is formatted (#595).
    pub unready: bool,
}

impl SeenDrive {
    /// `dev=/dev/sda;serial=..;wwn=..;model=..;shelf=..;position=..;bay=..;
    /// bytes=..;removable=0;usb=0;carries=..;running=0` (the initramfs
    /// strips `;` from what it reads off a drive).
    pub fn parse(s: &str) -> Result<SeenDrive, String> {
        let mut d = SeenDrive::default();
        for kv in s.split(';') {
            let Some((k, v)) = kv.split_once('=') else {
                if kv.trim().is_empty() {
                    continue;
                }
                return Err(format!("'{kv}' in --drive is not key=value"));
            };
            let v = v.trim().to_string();
            match k.trim() {
                "dev" => d.dev = v,
                "serial" => d.serial = v,
                "wwn" => d.wwn = v,
                "model" => d.model = v,
                "shelf" => d.shelf = if v == "-" { String::new() } else { v },
                "position" => d.position = v,
                "bay" => d.bay = v,
                "bytes" => d.bytes = v.parse().map_err(|_| format!("bytes={v} is not a number"))?,
                "removable" => d.removable = v == "1",
                "usb" => d.usb = v == "1",
                "carries" => d.carries = v,
                "running" => d.running = v == "1",
                "ready" => d.unready = v == "0",
                o => return Err(format!("--drive has '{o}', which is not known")),
            }
        }
        if d.dev.is_empty() {
            return Err(format!("--drive '{s}' has no dev="));
        }
        Ok(d)
    }

    fn external(&self) -> bool {
        self.position == "external"
    }

    /// How a person finds it: the bay and serial, then the device.
    pub fn said(&self) -> String {
        let mut s = self.dev.clone();
        let _ = write!(
            s,
            " ({} shelf{}, bay {}, serial {}, {})",
            if self.position.is_empty() { "unknown" } else { &self.position },
            if self.shelf.is_empty() { String::new() } else { format!(" {}", self.shelf) },
            if self.bay.is_empty() { "none" } else { &self.bay },
            if self.serial.is_empty() { "unknown" } else { &self.serial },
            if self.unready {
                format!("{}not ready: no readable format", if self.wwn.is_empty() { String::new() } else { format!("{}, ", self.wwn) })
            } else {
                format!("{} GB", self.bytes / 1_000_000_000)
            }
        );
        s
    }

    /// The number in a bay's name ("bay 03" is 3, "0" is 0), if it has one.
    fn number(s: &str) -> Option<u64> {
        s.chars().filter(|c| c.is_ascii_digit()).collect::<String>().parse().ok()
    }

    fn bay_is(&self, v: &str) -> bool {
        let b = self.bay.to_ascii_lowercase();
        let v = v.to_ascii_lowercase();
        !b.is_empty() && (b == v || (v.chars().all(|c| c.is_ascii_digit())
            && Self::number(&v).is_some() && Self::number(&b) == Self::number(&v)))
    }

    fn matches(&self, entry: &[(String, String)]) -> bool {
        entry.iter().all(|(k, v)| {
            let lv = v.trim().to_ascii_lowercase();
            match k.as_str() {
                "serial" => !self.serial.is_empty() && self.serial.to_ascii_lowercase() == lv,
                "wwn" => {
                    let w = self.wwn.to_ascii_lowercase();
                    let lv = lv.trim_start_matches("0x");
                    !w.is_empty() && (w == lv || w.ends_with(lv))
                }
                "shelf" => {
                    (!self.shelf.is_empty() && self.shelf.to_ascii_lowercase() == lv) || self.position == lv
                }
                "bay" => self.bay_is(&lv),
                // A bare entry: a serial, else a bay.
                _ => (!self.serial.is_empty() && self.serial.to_ascii_lowercase() == lv) || self.bay_is(&lv),
            }
        })
    }

    /// The order system legs are taken in: by bay, then serial.
    fn order_key(&self) -> (u8, u64, String, String, String) {
        let pos = match self.position.as_str() {
            "internal" => 0,
            "front" => 1,
            "rear" => 2,
            "external" => 4,
            _ => 3,
        };
        let bay_n = Self::number(&self.bay).unwrap_or(u64::MAX);
        (pos, bay_n, self.bay.clone(), self.serial.clone(), self.dev.clone())
    }
}

/// What the install does with the drives.
#[derive(Debug, Clone, PartialEq)]
pub struct StoragePlan {
    pub spec: StorageSpec,
    /// Every drive taken, by bay then serial.
    pub drives: Vec<SeenDrive>,
    /// The drives carrying a system leg (the first `system_mirror`).
    pub system: Vec<String>,
    /// The data set: None when one drive or `data_raid: none`.
    pub raid: Option<DataRaid>,
    /// What the plan says on the console, a line each.
    pub said: Vec<String>,
    /// The link-rate policy (#642): (the drive, Gbit/s).
    pub link_rates: Vec<(String, f32)>,
}

/// Choose the drives and check the section against them. `force` is the
/// wipe word on the command line (`rd.stormblock.assimilate=force`; an
/// install ticket is not it, #609); `storage.wipe` is the same word for one
/// install (#614). A drive that carries something is taken under
/// `drives: all` only with one of them.
pub fn plan(spec: &StorageSpec, seen: &[SeenDrive], force: bool) -> Result<StoragePlan, String> {
    let force = force || spec.wipe.is_some();
    let mut said = Vec::new();
    let mut link_rates: Vec<(String, f32)> = Vec::new();
    let mut taken: Vec<SeenDrive> = Vec::new();
    match &spec.drives {
        DriveSel::All => {
            let mut refused = Vec::new();
            for d in seen {
                // Never silent (#597): a drive left out is said with why.
                if d.usb {
                    said.push(format!("{} is a USB drive: not taken unless storage.drives lists it", d.said()));
                    continue;
                }
                if d.removable {
                    said.push(format!("{} is removable: not taken unless storage.drives lists it", d.said()));
                    continue;
                }
                if d.bytes == 0 && !d.unready {
                    said.push(format!("{} reads 0 bytes (no media): not taken", d.said()));
                    continue;
                }
                if d.external() {
                    said.push(format!("{} is on an external shelf: not taken unless storage.drives lists it", d.said()));
                    continue;
                }
                if d.running {
                    refused.push(format!("{} is the drive this boot runs from", d.said()));
                    continue;
                }
                if !d.carries.is_empty() && !force {
                    refused.push(format!("{} carries {}", d.said(), d.carries));
                    continue;
                }
                taken.push(d.clone());
            }
            if !refused.is_empty() {
                return Err(format!(
                    "storage.drives is all, and these drives are not taken: {}. Name them in storage.drives \
                     to take them, or install with the wipe word (storage.wipe: all in this install's install-config, \
                     or rd.stormblock.assimilate=force)",
                    refused.join("; ")
                ));
            }
        }
        DriveSel::Listed(entries) => {
            for (ei, e) in entries.iter().enumerate() {
                let said_e = e.iter().map(|(k, v)| if k == "any" { v.clone() } else { format!("{k}={v}") })
                    .collect::<Vec<_>>().join(" ");
                let m: Vec<&SeenDrive> = seen.iter().filter(|d| (d.bytes > 0 || d.unready) && d.matches(e)).collect();
                match m.as_slice() {
                    [] => return Err(format!("storage.drives names '{said_e}', which matches no drive on this machine")),
                    [d] => {
                        if d.running {
                            return Err(format!("storage.drives names '{said_e}': {}, the drive this boot runs from", d.said()));
                        }
                        if taken.iter().any(|t| t.dev == d.dev) {
                            return Err(format!("storage.drives names {} twice", d.said()));
                        }
                        if !d.carries.is_empty() {
                            said.push(format!("{} carries {}: taken, storage.drives names it", d.said(), d.carries));
                        }
                        if let Some((_, g)) = spec.link_rates.iter().find(|(i, _)| *i == ei) {
                            said.push(format!("{}: its link held at {g} Gbit (storage.drives' linkRate, #642)", d.said()));
                            link_rates.push((d.dev.clone(), *g));
                        }
                        taken.push((*d).clone());
                    }
                    many => {
                        return Err(format!(
                            "storage.drives names '{said_e}', which matches {} drives: {}",
                            many.len(),
                            many.iter().map(|d| d.said()).collect::<Vec<_>>().join("; ")
                        ))
                    }
                }
            }
        }
    }
    if taken.is_empty() {
        return Err("storage: no drive on this machine is taken (none on its own shelves)".into());
    }
    taken.sort_by_key(|d| d.order_key());
    let n = taken.len();
    let legs = match spec.system_mirror {
        Mirror::All => n,
        Mirror::N(k) if k > n => {
            return Err(format!("storage.system_mirror is {k}, and {n} drive{} taken", if n == 1 { " is" } else { "s are" }))
        }
        Mirror::N(k) => k,
    };
    let system: Vec<String> = taken.iter().take(legs).map(|d| d.dev.clone()).collect();
    let raid = match spec.data_raid {
        _ if n == 1 => {
            said.push("one drive: the data slab is on its data region, no set".into());
            None
        }
        None => {
            said.push("data_raid: none: a data slab on each drive's data region, no set".into());
            None
        }
        Some(r) => {
            let members = n as u32 - r.spares.min(n as u32);
            if r.spares as usize >= n || members < r.parity + 1 || (r.parity == 0 && members < 2) {
                return Err(format!(
                    "storage.data_raid is parity {} with {} spare{} over {n} drives: {members} members cannot hold it",
                    r.parity,
                    r.spares,
                    if r.spares == 1 { "" } else { "s" }
                ));
            }
            if r.parity == 0 {
                said.push("data_raid.parity is 0: a striped set; one drive lost loses every volume on it".into());
            }
            Some(r)
        }
    };
    let unready: Vec<&SeenDrive> = taken.iter().filter(|d| d.unready).collect();
    if !unready.is_empty() {
        said.push(format!(
            "{} drive{} not ready (no readable format): formatted first, then planned again with {} size{}",
            unready.len(),
            if unready.len() == 1 { " is" } else { "s are" },
            if unready.len() == 1 { "its" } else { "their" },
            if unready.len() == 1 { "" } else { "s" }
        ));
    }
    let ready = taken.iter().filter(|d| !d.unready);
    let smallest = ready.clone().map(|d| d.bytes).min().unwrap_or(0);
    let largest = ready.map(|d| d.bytes).max().unwrap_or(0);
    if largest > smallest {
        said.push(format!(
            "the drives differ in size: every data region is the smallest's, {} GB of the largest left unused",
            (largest - smallest) / 1_000_000_000
        ));
    }
    Ok(StoragePlan { spec: spec.clone(), drives: taken, system, raid, said, link_rates })
}

/// `'...'` for sh: the value as one word, `'` written `'\''`.
pub fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

impl StoragePlan {
    /// The plan as shell assignments `/init` sources.
    pub fn to_shell(&self) -> String {
        let mut o = String::new();
        let devs: Vec<&str> = self.drives.iter().map(|d| d.dev.as_str()).collect();
        let _ = writeln!(o, "STORAGE_DRIVES={}", sh_quote(&devs.join(" ")));
        let _ = writeln!(o, "STORAGE_SYSTEM={}", sh_quote(&self.system.join(" ")));
        let unready: Vec<&str> = self.drives.iter().filter(|d| d.unready).map(|d| d.dev.as_str()).collect();
        let _ = writeln!(o, "STORAGE_UNREADY={}", sh_quote(&unready.join(" ")));
        let _ = writeln!(o, "STORAGE_BLOCK_SIZE={}", self.spec.block_size);
        let _ = writeln!(o, "STORAGE_PROTECTION={}", self.spec.protection.as_str());
        let _ = writeln!(o, "STORAGE_METHOD={}", self.spec.method);
        let _ = writeln!(o, "STORAGE_SYSTEM_BYTES={}", self.spec.system_size.unwrap_or(0));
        match self.raid {
            Some(r) => {
                let _ = writeln!(o, "STORAGE_RAID=yes");
                let _ = writeln!(o, "STORAGE_PARITY={}", r.parity);
                let _ = writeln!(o, "STORAGE_SPARES={}", r.spares);
            }
            None => {
                let _ = writeln!(o, "STORAGE_RAID=no");
                let _ = writeln!(o, "STORAGE_PARITY=");
                let _ = writeln!(o, "STORAGE_SPARES=");
            }
        }
        let _ = writeln!(
            o,
            "STORAGE_SMALLEST={}",
            self.drives.iter().filter(|d| !d.unready).map(|d| d.bytes).min().unwrap_or(0)
        );
        let mut said = vec![format!(
            "STORAGE: {} drive{}, each formatted {} + PI {} ({}), a system leg on {}, data {}",
            self.drives.len(),
            if self.drives.len() == 1 { "" } else { "s" },
            self.spec.block_size,
            self.spec.protection.as_str(),
            self.spec.method,
            self.system.len(),
            match self.raid {
                Some(r) => format!(
                    "on one stormraid set ({} data + {} parity{})",
                    self.drives.len() as u32 - r.spares - r.parity,
                    r.parity,
                    if r.spares > 0 { format!(" + {} spare", r.spares) } else { String::new() }
                ),
                None => "on each drive's region".into(),
            }
        )];
        for d in &self.drives {
            said.push(format!(
                "  {}{}",
                d.said(),
                if self.system.contains(&d.dev) { ": system leg" } else { "" }
            ));
        }
        said.extend(self.said.iter().map(|s| format!("  {s}")));
        let _ = writeln!(o, "STORAGE_SAID={}", sh_quote(&said.join("\n")));
        // The wipe word from the install-config (#614): /init takes it as
        // STORAGE_FORCE for this install and says where it came from.
        let _ = writeln!(o, "STORAGE_WIPE={}", sh_quote(&self.spec.wipe.as_ref().map(|w| w.said()).unwrap_or_default()));
        let _ = writeln!(o, "STORAGE_ON_DRIVE_FAILURE={}", self.spec.on_drive_failure.as_str());
        // #623: the running node does a fresh install.
        let _ = writeln!(o, "STORAGE_RUNTIME={}", if self.spec.runtime { "1" } else { "" });
        let lr: Vec<String> = self.link_rates.iter().map(|(d, g)| format!("{d}={g}")).collect();
        let _ = writeln!(o, "STORAGE_LINKRATE={}", sh_quote(&lr.join(" ")));
        let _ = writeln!(o, "STORAGE_STOP=");
        o
    }
}

/// The whole step: the install-config's text (read for `storage:` and
/// `system_drive:` only) and the drives → shell. `Ok(None)` when it has no
/// `storage:` section: today's rules apply (#573, option A).
pub fn plan_from_config(doc: &str, seen: &[SeenDrive], force: bool) -> Result<Option<String>, String> {
    let storage = match top_level(doc, "storage") {
        Ok(None) => return Ok(None),
        Ok(Some(v)) => v,
        Err(e) => return Err(format!("storage in the install-config cannot be read: {e}")),
    };
    if !matches!(top_level(doc, "system_drive"), Ok(None)) {
        return Err("the install-config has both storage: and system_drive: - one or the other (#573): \
                    storage: lays every drive it takes, system_drive: names the one drive an install takes"
            .into());
    }
    let spec = StorageSpec::from_yaml(&storage)?;
    plan(&spec, seen, force).map(|p| Some(p.to_shell()))
}

/// The install-config's words the running node keeps (#623): `mode`,
/// `system_drive` and `storage` without its `wipe` (the wipe word is for one
/// install, and it never leaves the initramfs). Nothing else: the rest of
/// the file holds secrets (#275), and `/init` deletes it before
/// `switch_root` (#685). Comments are dropped. Empty when it has none of
/// them.
pub fn install_intent(doc: &str) -> String {
    const KEEP: [&str; 3] = ["mode", "storage", "system_drive"];
    let mut out = String::new();
    // The top-level key being copied, and in `storage` the indent of its
    // children and whether a `wipe` child is being skipped.
    let mut keeping = false;
    let mut in_storage = false;
    let mut child_indent: Option<usize> = None;
    let mut skipping_wipe = false;
    for raw in doc.lines() {
        let l = strip_comment(raw).trim_end();
        let t = l.trim_start();
        if t.is_empty() || t == "---" {
            continue;
        }
        let indent = l.len() - t.len();
        if indent == 0 {
            let key = split_key(t).map(|(k, _)| k);
            keeping = key.as_deref().is_some_and(|k| KEEP.contains(&k));
            in_storage = key.as_deref() == Some("storage");
            child_indent = None;
            skipping_wipe = false;
            if keeping {
                let _ = writeln!(out, "{l}");
            }
            continue;
        }
        if !keeping {
            continue;
        }
        if in_storage {
            let ci = *child_indent.get_or_insert(indent);
            if indent <= ci {
                skipping_wipe = indent == ci && split_key(t).is_some_and(|(k, _)| k == "wipe");
            }
            if skipping_wipe {
                continue;
            }
        }
        let _ = writeln!(out, "{l}");
    }
    out
}

/// `/sys/block/<name>`'s shelf, as `/init`'s `shelf_of` reads it (#573):
/// (shelf id, position, bay).
fn shelf_in(block: &std::path::Path) -> (String, String, String) {
    let real = std::fs::canonicalize(block).map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
    let expander = real.contains("/expander-");
    let comp = std::fs::read_dir(block.join("device")).ok().and_then(|rd| {
        rd.flatten().filter_map(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            n.strip_prefix("enclosure_device:").map(|b| (b.to_string(), e.path()))
        }).last()
    });
    let Some((bay, link)) = comp else {
        return ("-".into(), if expander { "external" } else { "internal" }.into(), String::new());
    };
    let clean = |s: String| -> String {
        s.chars().filter(|c| *c != '"' && *c != '\\').map(|c| if c.is_control() { ' ' } else { c }).collect::<String>().trim().to_string()
    };
    let read = |p: std::path::PathBuf| std::fs::read_to_string(p).map(clean).unwrap_or_default();
    let edir = std::fs::canonicalize(&link).ok().and_then(|p| p.parent().map(|p| p.to_path_buf()));
    let (id, ident) = match &edir {
        Some(e) => {
            let id = read(e.join("id"));
            let id = if id.is_empty() { e.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default() } else { id };
            let ident = clean(format!("{} {}", read(e.join("device/vendor")), read(e.join("device/model"))));
            (id, ident)
        }
        None => (String::new(), String::new()),
    };
    let both = format!("{bay} {ident}");
    let lower = both.to_ascii_lowercase();
    let position = if expander && !(ident.starts_with("DP BP") || ident.starts_with("DELL BP")) {
        "external"
    } else if lower.contains("rear") {
        "rear"
    } else if lower.contains("internal") || both.contains("M.2") || both.contains("BOSS") {
        "internal"
    } else {
        "front"
    };
    (id, position.into(), bay)
}

/// Every whole disk with media under `sys` (`/sys` on a node), as `/init`'s
/// `storage_disks` lists them and `storage_plan` describes them (#623: the
/// running node plans as `/init` does). `carries` says what a disk holds
/// that is not this node's own (empty for blank or this node's); nothing on
/// a running node is `running` (the node runs from its volumes, not a
/// drive), and a drive with no readable format is not listed (#595: the
/// card's rows say those).
pub fn seen_in(sys: &std::path::Path, carries: impl Fn(&str) -> String) -> Vec<SeenDrive> {
    let mut names: Vec<String> = std::fs::read_dir(sys.join("block"))
        .map(|rd| rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect())
        .unwrap_or_default();
    names.sort();
    let mut out = Vec::new();
    for n in names {
        const NOT: [&str; 11] = ["loop", "ram", "zram", "ublkb", "ublkc", "nbd", "dm-", "md", "sr", "fd", "zd"];
        if NOT.iter().any(|p| n.starts_with(p)) {
            continue;
        }
        let b = sys.join("block").join(&n);
        if !b.join("device").exists() {
            continue;
        }
        let sectors: u64 = std::fs::read_to_string(b.join("size")).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0);
        if sectors == 0 {
            continue;
        }
        let real = std::fs::canonicalize(&b).map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
        let id = crate::drive::identity::of_in(sys, &n).unwrap_or_default();
        let (shelf, position, bay) = shelf_in(&b);
        let dev = format!("/dev/{n}");
        out.push(SeenDrive {
            carries: carries(&dev),
            dev,
            serial: id.serial,
            wwn: id.wwn,
            model: id.model,
            shelf,
            position,
            bay,
            bytes: sectors * 512,
            removable: std::fs::read_to_string(b.join("removable")).map(|s| s.trim() == "1").unwrap_or(false),
            usb: real.contains("/usb"),
            running: false,
            unready: false,
        });
    }
    out
}

impl StoragePlan {
    /// The plan as the node's NodeInstall status shows it (#623): the
    /// drives taken, their legs, the format and the set, and what the plan
    /// says. Whether a wipe was asked, never the word.
    pub fn to_json(&self) -> serde_json::Value {
        let drives: Vec<serde_json::Value> = self
            .drives
            .iter()
            .map(|d| {
                serde_json::json!({
                    "dev": d.dev, "serial": d.serial, "wwn": d.wwn, "model": d.model,
                    "shelf": d.shelf, "position": d.position, "bay": d.bay, "bytes": d.bytes,
                    "notReady": d.unready, "systemLeg": self.system.contains(&d.dev),
                })
            })
            .collect();
        serde_json::json!({
            "drives": drives,
            "systemLegs": self.system,
            "blockSize": self.spec.block_size,
            "protection": self.spec.protection.as_str(),
            "method": self.spec.method,
            "set": self.raid.map(|r| serde_json::json!({"parity": r.parity, "spares": r.spares})),
            "onDriveFailure": self.spec.on_drive_failure.as_str(),
            "runtime": self.spec.runtime,
            "wipeAsked": self.spec.wipe.is_some(),
            "said": self.said,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #623: what the running node keeps of an install-config: mode,
    /// system_drive and storage, never the wipe word or anything else.
    #[test]
    fn the_install_intent_keeps_no_secret_and_no_wipe_word() {
        let doc = "pullSecret: '{\"auths\": {}}'\nmode: install   # the default\nstorage:\n  drives: all\n  wipe:\n    by: owner\n    why: forge1 again\n  format:\n    block_size: 4096\n  data_raid:\n    parity: 1\napiToken: secret\nsystem_drive: sda\n";
        let kept = install_intent(doc);
        assert_eq!(kept, "mode: install\nstorage:\n  drives: all\n  format:\n    block_size: 4096\n  data_raid:\n    parity: 1\nsystem_drive: sda\n");
        assert!(!kept.contains("secret") && !kept.contains("auths") && !kept.contains("owner") && !kept.contains('#'));
        // Still the same storage section, less the word.
        let spec = StorageSpec::from_yaml(&top_level(&kept, "storage").unwrap().unwrap()).unwrap();
        assert!(spec.wipe.is_none());
        assert_eq!(spec.data_raid, Some(DataRaid { parity: 1, spares: 0 }));
        let inline = install_intent("storage:\n  wipe: all\n  drives: all\n");
        assert_eq!(inline, "storage:\n  drives: all\n");
        assert_eq!(install_intent("apiToken: x\n"), "");
    }

    /// #623: the drives as /init describes them, read from sysfs by the
    /// running node: disks with media only, their identity, shelf and bay.
    #[test]
    fn the_running_node_sees_its_drives_as_init_does() {
        let w = tempfile::tempdir().unwrap();
        let sys = w.path();
        let mk = |name: &str, sectors: u64, model: &str, serial: &str| {
            let b = sys.join("block").join(name);
            std::fs::create_dir_all(b.join("device")).unwrap();
            std::fs::write(b.join("size"), format!("{sectors}\n")).unwrap();
            std::fs::write(b.join("removable"), "0\n").unwrap();
            std::fs::write(b.join("device/model"), format!("{model}\n")).unwrap();
            std::fs::write(b.join("device/serial"), format!("{serial}\n")).unwrap();
            std::fs::create_dir_all(sys.join("class/block")).unwrap();
            std::os::unix::fs::symlink(&b, sys.join("class/block").join(name)).unwrap();
            b
        };
        mk("sda", 19_532_873_728, "HUH721010AL4200", "7JG1");
        let sdb = mk("sdb", 19_532_873_728, "HUH721010AL4200", "7JG2");
        mk("sdc", 0, "EMPTY", "0");
        mk("loop0", 2048, "loop", "-");
        mk("ublkb65535", 2048, "set", "-");
        // An enclosure slot for sdb.
        let enc = sys.join("class/enclosure/0:0:8:0");
        std::fs::create_dir_all(enc.join("Slot 03")).unwrap();
        std::fs::create_dir_all(enc.join("device")).unwrap();
        std::fs::write(enc.join("id"), "0x500056b3\n").unwrap();
        std::fs::write(enc.join("device/vendor"), "DP\n").unwrap();
        std::fs::write(enc.join("device/model"), "BP14G+\n").unwrap();
        std::os::unix::fs::symlink(enc.join("Slot 03"), sdb.join("device/enclosure_device:Slot 03")).unwrap();
        let seen = seen_in(sys, |d| if d == "/dev/sdb" { "a partition table".into() } else { String::new() });
        let devs: Vec<&str> = seen.iter().map(|d| d.dev.as_str()).collect();
        assert_eq!(devs, ["/dev/sda", "/dev/sdb"], "media only; never loop, ublk or the set");
        assert_eq!(seen[0].bytes, 19_532_873_728 * 512);
        assert_eq!((seen[0].serial.as_str(), seen[0].model.as_str()), ("7JG1", "HUH721010AL4200"));
        assert_eq!((seen[0].shelf.as_str(), seen[0].position.as_str(), seen[0].bay.as_str()), ("-", "internal", ""));
        assert_eq!(seen[1].bay, "Slot 03");
        assert_eq!(seen[1].shelf, "0x500056b3");
        assert_eq!(seen[1].position, "front");
        assert_eq!(seen[1].carries, "a partition table");
        assert!(seen.iter().all(|d| !d.running && !d.usb && !d.removable));
    }

    /// #623: the plan as the NodeInstall shows it: whether a wipe was asked,
    /// never the word.
    #[test]
    fn the_plan_as_json_says_a_wipe_was_asked_not_its_word() {
        let doc = "storage:\n  drives: all\n  wipe: {by: owner, why: test}\n";
        let spec = StorageSpec::from_yaml(&top_level(doc, "storage").unwrap().unwrap()).unwrap();
        let seen: Vec<SeenDrive> = (0..4)
            .map(|i| SeenDrive::parse(&format!("dev=/dev/sd{};serial=S{i};bay={i};position=front;bytes=10000000000000", ['a', 'b', 'c', 'd'][i])).unwrap())
            .collect();
        let p = plan(&spec, &seen, false).unwrap();
        let j = p.to_json();
        assert_eq!(j["drives"].as_array().unwrap().len(), 4);
        assert_eq!(j["wipeAsked"], serde_json::json!(true));
        assert!(!j.to_string().contains("owner"), "{j}");
        assert_eq!(j["blockSize"], serde_json::json!(4096));
    }

    fn s(v: &str) -> Yaml {
        Yaml::Str(v.into())
    }

    #[test]
    fn reads_the_contract_example() {
        let doc = "pullSecret: '{\"auths\": {}}'\nstorage:\n  drives: all                 # all (default)\n  format:\n    block_size: 4096\n    protection: type1         # none | type1\n    method: auto\n  system_mirror: all\n  system_size: release\n  data_raid:                  # or none\n    parity: 1\n    spares: 0\napiToken: secret\n";
        let v = top_level(doc, "storage").unwrap().unwrap();
        let spec = StorageSpec::from_yaml(&v).unwrap();
        assert_eq!(spec, StorageSpec::default());
        assert_eq!(top_level(doc, "apiToken").unwrap(), Some(s("secret")));
        assert_eq!(top_level(doc, "system_drive").unwrap(), None);
    }

    #[test]
    fn reads_flow_and_lists() {
        let v = parse("a: {block_size: 512, protection: none}\nb: [X1, 'bay 3', \"Z\"]\nc:\n- one\n- serial: S2\n  bay: 4\nd:\n  - [1, 2]\n").unwrap();
        let Yaml::Map(m) = v else { panic!() };
        assert_eq!(m[0].1, Yaml::Map(vec![("block_size".into(), s("512")), ("protection".into(), s("none"))]));
        assert_eq!(m[1].1, Yaml::List(vec![s("X1"), s("bay 3"), s("Z")]));
        assert_eq!(
            m[2].1,
            Yaml::List(vec![s("one"), Yaml::Map(vec![("serial".into(), s("S2")), ("bay".into(), s("4"))])])
        );
        assert_eq!(m[3].1, Yaml::List(vec![Yaml::List(vec![s("1"), s("2")])]));
    }

    #[test]
    fn refuses_what_it_does_not_know() {
        let bad = |doc: &str| {
            let v = top_level(doc, "storage").unwrap().unwrap();
            StorageSpec::from_yaml(&v).unwrap_err()
        };
        assert!(bad("storage:\n  drive: all\n").contains("'drive'"));
        assert!(bad("storage:\n  format: {protection: type2}\n").contains("type2"));
        assert!(bad("storage:\n  format: {block_size: 520}\n").contains("520"));
        assert!(bad("storage:\n  system_mirror: 0\n").contains("0"));
        assert!(bad("storage:\n  data_raid: {parity: 3}\n").contains("3"));
        assert!(bad("storage:\n  format: {method: slow}\n").contains("slow"));
    }

    #[test]
    fn system_size_and_none() {
        let v = top_level("storage:\n  system_size: 128Gi\n  data_raid: none\n", "storage").unwrap().unwrap();
        let spec = StorageSpec::from_yaml(&v).unwrap();
        assert_eq!(spec.system_size, Some(128 << 30));
        assert_eq!(spec.data_raid, None);
        let v = top_level("storage: {}\n", "storage").unwrap().unwrap();
        assert_eq!(StorageSpec::from_yaml(&v).unwrap(), StorageSpec::default());
        let v = top_level("storage:\n", "storage").unwrap().unwrap();
        assert_eq!(StorageSpec::from_yaml(&v).unwrap(), StorageSpec::default());
    }

    fn drive(dev: &str, bay: &str, serial: &str, tb: u64) -> SeenDrive {
        SeenDrive {
            dev: dev.into(),
            serial: serial.into(),
            position: "front".into(),
            bay: bay.into(),
            bytes: tb * 1_000_000_000_000,
            ..Default::default()
        }
    }

    fn forge1() -> Vec<SeenDrive> {
        vec![drive("/dev/sdc", "2", "S2", 10), drive("/dev/sda", "0", "S0", 10), drive("/dev/sdd", "3", "S3", 10),
             drive("/dev/sdb", "1", "S1", 10)]
    }

    #[test]
    fn forge1_with_the_defaults() {
        let p = plan(&StorageSpec::default(), &forge1(), false).unwrap();
        let devs: Vec<&str> = p.drives.iter().map(|d| d.dev.as_str()).collect();
        assert_eq!(devs, ["/dev/sda", "/dev/sdb", "/dev/sdc", "/dev/sdd"]);
        assert_eq!(p.system, ["/dev/sda", "/dev/sdb", "/dev/sdc", "/dev/sdd"]);
        assert_eq!(p.raid, Some(DataRaid { parity: 1, spares: 0 }));
        let sh = p.to_shell();
        assert!(sh.contains("STORAGE_DRIVES='/dev/sda /dev/sdb /dev/sdc /dev/sdd'\n"));
        assert!(sh.contains("STORAGE_RAID=yes\nSTORAGE_PARITY=1\n"));
        assert!(sh.contains("3 data + 1 parity"), "{sh}");
        assert!(sh.contains("STORAGE_STOP=\n"));
    }

    /// pvetest3, 12.81 (#597): one SCSI disk and three virtio disks, none
    /// with a serial, from the --drive words /init passes, under the
    /// install-config line the appliance wrote.
    #[test]
    fn pvetest3_four_disks_at_512_all_taken() {
        let doc = "hostname: pvetest3\nstorage: {\"data_raid\":{\"parity\":1},\"drives\":\"all\",\"format\":{\"block_size\":512,\"protection\":\"none\"},\"system_mirror\":\"all\"}\n";
        let w = |dev: &str| format!("dev={dev};serial=;wwn=;model=;shelf=-;position=internal;bay=;bytes=68719476736;removable=0;usb=0;carries=;running=0");
        let seen: Vec<SeenDrive> =
            ["/dev/sda", "/dev/vda", "/dev/vdb", "/dev/vdc"].iter().map(|d| SeenDrive::parse(&w(d)).unwrap()).collect();
        let sh = plan_from_config(doc, &seen, true).unwrap().unwrap();
        assert!(sh.contains("STORAGE_DRIVES='/dev/sda /dev/vda /dev/vdb /dev/vdc'\n"), "{sh}");
        assert!(sh.contains("STORAGE_SYSTEM='/dev/sda /dev/vda /dev/vdb /dev/vdc'\n"), "{sh}");
        assert!(sh.contains("STORAGE_RAID=yes\nSTORAGE_PARITY=1\n"), "{sh}");
        assert!(sh.contains("STORAGE_BLOCK_SIZE=512\n") && sh.contains("STORAGE_PROTECTION=none\n"), "{sh}");
        assert!(sh.contains("3 data + 1 parity"), "{sh}");
    }

    #[test]
    fn all_names_every_drive_it_leaves_out() {
        let mut d = forge1();
        d.push(SeenDrive { usb: true, ..drive("/dev/sde", "", "U1", 1) });
        d.push(SeenDrive { removable: true, ..drive("/dev/sdf", "", "R1", 1) });
        d.push(SeenDrive { bytes: 0, ..drive("/dev/sdg", "", "Z1", 1) });
        let p = plan(&StorageSpec::default(), &d, false).unwrap();
        assert_eq!(p.drives.len(), 4);
        for (dev, why) in [("/dev/sde", "USB"), ("/dev/sdf", "removable"), ("/dev/sdg", "0 bytes")] {
            assert!(p.said.iter().any(|s| s.contains(dev) && s.contains(why)), "{dev}: {:?}", p.said);
        }
    }

    #[test]
    fn mirror_n_by_bay_then_serial() {
        let spec = StorageSpec { system_mirror: Mirror::N(2), ..Default::default() };
        let p = plan(&spec, &forge1(), false).unwrap();
        assert_eq!(p.system, ["/dev/sda", "/dev/sdb"]);
        let spec = StorageSpec { system_mirror: Mirror::N(5), ..Default::default() };
        assert!(plan(&spec, &forge1(), false).unwrap_err().contains("system_mirror is 5"));
    }

    #[test]
    fn all_refuses_what_carries_something_unless_forced() {
        let mut d = forge1();
        d[2].carries = "a stormblock install".into();
        let e = plan(&StorageSpec::default(), &d, false).unwrap_err();
        assert!(e.contains("/dev/sdd") && e.contains("a stormblock install") && e.contains("assimilate=force"), "{e}");
        assert_eq!(plan(&StorageSpec::default(), &d, true).unwrap().drives.len(), 4);
    }

    /// #614: forge1's four drives, sda holding this node's slabs, an
    /// operator's `wipe:` in the install-config takes it; without the word
    /// the same plan refuses it.
    #[test]
    fn the_install_config_wipe_word_takes_what_carries_something() {
        let mut d = forge1();
        d[1].carries = "stormblock slabs (an install)".into();
        let without = "storage: {\"drives\":\"all\"}\n";
        let e = plan_from_config(without, &d, false).unwrap_err();
        assert!(e.contains("/dev/sda") && e.contains("storage.wipe: all"), "{e}");
        for doc in [
            "storage:\n  drives: all\n  wipe: all\n",
            "storage: {\"drives\":\"all\",\"wipe\":true}\n",
            "storage: {\"drives\":\"all\",\"wipe\":{\"by\":\"testhost install --force by ops\",\"why\":\"four drives\"}}\n",
        ] {
            let sh = plan_from_config(doc, &d, false).unwrap().unwrap();
            assert!(sh.contains("STORAGE_DRIVES='/dev/sda /dev/sdb /dev/sdc /dev/sdd'\n"), "{sh}");
            assert!(sh.contains("STORAGE_WIPE='the install-config"), "{sh}");
        }
        let sh = plan_from_config(
            "storage: {\"wipe\":{\"by\":\"testhost install --force by ops\",\"why\":\"four drives\"}}\n",
            &d,
            false,
        )
        .unwrap()
        .unwrap();
        assert!(sh.contains("STORAGE_WIPE='the install-config (testhost install --force by ops: four drives)'\n"), "{sh}");
        // No word, or a word that says no: nothing to wipe, said empty.
        for doc in ["storage:\n  wipe: false\n", "storage:\n  drives: all\n"] {
            let sh = plan_from_config(doc, &forge1(), false).unwrap().unwrap();
            assert!(sh.contains("STORAGE_WIPE=''\n"), "{sh}");
        }
        let v = top_level("storage:\n  wipe: some\n", "storage").unwrap().unwrap();
        assert!(StorageSpec::from_yaml(&v).unwrap_err().contains("storage.wipe is 'some'"));
        let v = top_level("storage:\n  wipe: {who: x}\n", "storage").unwrap().unwrap();
        assert!(StorageSpec::from_yaml(&v).unwrap_err().contains("'who'"));
    }

    #[test]
    fn external_only_when_listed() {
        let mut d = forge1();
        d.push(SeenDrive { position: "external".into(), ..drive("/dev/sde", "7", "X7", 4) });
        let p = plan(&StorageSpec::default(), &d, false).unwrap();
        assert_eq!(p.drives.len(), 4);
        assert!(p.said.iter().any(|s| s.contains("/dev/sde") && s.contains("external")));
        let spec = StorageSpec {
            drives: DriveSel::Listed(vec![vec![("any".into(), "x7".into())], vec![("bay".into(), "0".into())]]),
            ..Default::default()
        };
        let p = plan(&spec, &d, false).unwrap();
        let devs: Vec<&str> = p.drives.iter().map(|d| d.dev.as_str()).collect();
        assert_eq!(devs, ["/dev/sda", "/dev/sde"]);
    }

    /// #625: a drive that cannot be laid stops the install unless the
    /// install-config says to go on without it.
    #[test]
    fn on_drive_failure() {
        let sh = plan_from_config("storage:\n  drives: all\n", &forge1(), false).unwrap().unwrap();
        assert!(sh.contains("STORAGE_ON_DRIVE_FAILURE=stop\n"), "{sh}");
        let sh = plan_from_config("storage: {\"drives\":\"all\",\"on_drive_failure\":\"continue\"}\n", &forge1(), false)
            .unwrap()
            .unwrap();
        assert!(sh.contains("STORAGE_ON_DRIVE_FAILURE=continue\n"), "{sh}");
        let v = top_level("storage:\n  on_drive_failure: maybe\n", "storage").unwrap().unwrap();
        assert!(StorageSpec::from_yaml(&v).unwrap_err().contains("continue or stop"));
    }

    /// #623: `storage.runtime: true` reaches /init as STORAGE_RUNTIME=1 and
    /// the NodeInstall's plan; it is off unless said.
    #[test]
    fn the_runtime_switch() {
        let sh = plan_from_config("storage:\n  drives: all\n", &forge1(), false).unwrap().unwrap();
        assert!(sh.contains("STORAGE_RUNTIME=\n"), "{sh}");
        let sh = plan_from_config("storage:\n  drives: all\n  runtime: true\n", &forge1(), false).unwrap().unwrap();
        assert!(sh.contains("STORAGE_RUNTIME=1\n"), "{sh}");
        let v = top_level("storage:\n  runtime: true\n", "storage").unwrap().unwrap();
        assert!(StorageSpec::from_yaml(&v).unwrap().runtime);
        let v = top_level("storage:\n  runtime: maybe\n", "storage").unwrap().unwrap();
        assert!(StorageSpec::from_yaml(&v).unwrap_err().contains("true or false"));
        // The words the node keeps carry it.
        assert!(install_intent("storage:\n  runtime: true\n  wipe: all\n").contains("runtime: true"));
    }

    /// #642: `linkRate` on an entry is that drive's link policy, not a key
    /// to match it by.
    #[test]
    fn a_drives_link_rate() {
        let doc = "storage:\n  drives:\n    - bay: 0\n    - {bay: 1, linkRate: 6}\n";
        let sh = plan_from_config(doc, &forge1(), false).unwrap().unwrap();
        assert!(sh.contains("STORAGE_LINKRATE='/dev/sdb=6'\n"), "{sh}");
        assert!(sh.contains("its link held at 6 Gbit"), "{sh}");
        let sh = plan_from_config("storage:\n  drives: all\n", &forge1(), false).unwrap().unwrap();
        assert!(sh.contains("STORAGE_LINKRATE=''\n"), "{sh}");
        let v = top_level("storage:\n  drives:\n    - {bay: 1, linkRate: 7}\n", "storage").unwrap().unwrap();
        assert!(StorageSpec::from_yaml(&v).unwrap_err().contains("1.5, 3, 6, 12 or 22.5"));
    }

    #[test]
    fn listed_must_match_one() {
        let spec = StorageSpec { drives: DriveSel::Listed(vec![vec![("any".into(), "NOPE".into())]]), ..Default::default() };
        assert!(plan(&spec, &forge1(), false).unwrap_err().contains("matches no drive"));
    }

    #[test]
    fn one_drive_and_parity_bounds() {
        let p = plan(&StorageSpec::default(), &forge1()[..1], false).unwrap();
        assert_eq!(p.raid, None);
        let spec = StorageSpec { data_raid: Some(DataRaid { parity: 2, spares: 1 }), ..Default::default() };
        assert!(plan(&spec, &forge1()[..3], false).is_err());
        let spec = StorageSpec { data_raid: Some(DataRaid { parity: 1, spares: 1 }), ..Default::default() };
        assert!(plan(&spec, &forge1(), false).unwrap().to_shell().contains("2 data + 1 parity + 1 spare"));
    }

    #[test]
    fn sizes_differ_said() {
        let mut d = forge1();
        d[0].bytes = 12_000_000_000_000;
        let p = plan(&StorageSpec::default(), &d, false).unwrap();
        assert!(p.said.iter().any(|s| s.contains("2000 GB")), "{:?}", p.said);
    }

    #[test]
    fn storage_with_system_drive_refused() {
        let e = plan_from_config("system_drive:\n  serial: S0\nstorage:\n  drives: all\n", &forge1(), false).unwrap_err();
        assert!(e.contains("system_drive"));
        assert_eq!(plan_from_config("system_drive:\n  serial: S0\n", &forge1(), false).unwrap(), None);
    }

    #[test]
    fn seen_drive_parse() {
        let d = SeenDrive::parse("dev=/dev/sda;serial=S0;wwn=;model=X378;shelf=-;position=front;bay=0;bytes=10;removable=0;usb=0;carries=;running=0").unwrap();
        assert_eq!(d.dev, "/dev/sda");
        assert_eq!(d.shelf, "");
        assert_eq!(d.bytes, 10);
        assert!(SeenDrive::parse("serial=S0").is_err());
    }

    #[test]
    fn not_ready_drives_are_taken_and_formatted_first() {
        // forge1 (#595): sda readable, three unformatted drives behind sg.
        let mut d = vec![drive("/dev/sda", "0", "S0", 10)];
        for (dev, bay, wwn) in [("/dev/sg1", "1", "naa.5000cca26801b648"), ("/dev/sg2", "2", "naa.5000cca26a819e68"),
                                ("/dev/sg3", "3", "naa.5000cca268077300")] {
            let mut u = SeenDrive::parse(&format!(
                "dev={dev};serial=;wwn={wwn};model=H7210A520SUN010T;shelf=-;position=front;bay={bay};bytes=0;removable=0;usb=0;carries=;running=0;ready=0"
            ))
            .unwrap();
            assert!(u.unready);
            u.serial = format!("S{bay}");
            d.push(u);
        }
        let sh = plan_from_config("storage:\n  drives: all\n", &d, false).unwrap().unwrap();
        assert!(sh.contains("STORAGE_DRIVES='/dev/sda /dev/sg1 /dev/sg2 /dev/sg3'\n"), "{sh}");
        assert!(sh.contains("STORAGE_UNREADY='/dev/sg1 /dev/sg2 /dev/sg3'\n"), "{sh}");
        assert!(sh.contains("STORAGE_SMALLEST=10000000000000\n"), "{sh}");
        assert!(sh.contains("naa.5000cca26801b648, not ready: no readable format"), "{sh}");
        assert!(sh.contains("3 drives are not ready"), "{sh}");
        // Named by serial, a not-ready drive matches too.
        let sh = plan_from_config("storage:\n  drives: [S0, S2]\n", &d, false).unwrap().unwrap();
        assert!(sh.contains("STORAGE_UNREADY='/dev/sg2'\n"), "{sh}");
        // A drive with no media that answers as ready (a card reader) is still not taken.
        let mut e = d.clone();
        e[1].unready = false;
        let sh = plan_from_config("storage:\n  drives: all\n", &e, false).unwrap().unwrap();
        assert!(sh.contains("STORAGE_DRIVES='/dev/sda /dev/sg2 /dev/sg3'\n"), "{sh}");
        // Every drive ready: an empty list, said.
        let sh = plan_from_config("storage:\n  drives: all\n", &forge1(), false).unwrap().unwrap();
        assert!(sh.contains("STORAGE_UNREADY=''\n"), "{sh}");
    }

    #[test]
    fn sh_quote_quotes() {
        assert_eq!(sh_quote("a'b"), "'a'\\''b'");
    }
}
