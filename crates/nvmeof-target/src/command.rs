//! A host's pending command (#494, stormbootx#167): what stormbootx does at
//! its next boot, or while it holds, when the machine's BMC answers nothing.
//! One per host, beside the intent, on forge's host record and on
//! `stormboot-serve`'s; the same rules for both, so they live here.
//!
//! - `PUT …/command {command, args, id?, issued?, by?, host?, sig?}`: the verb
//!   is one of [`VERBS`]; `id` grows per host (a given one at or below one
//!   the host has had is refused, an absent one is minted: the next);
//!   `issued` is now when absent; `sig` is kept as given: stormbootx checks
//!   it over `stormbootx-command v1\n<host>\n<id>\n<command>\n<args>\n<issued>\n`
//!   (Ed25519, stormbootx#167), the engine never does. A `host` given must be
//!   the host's own name: the signature covers it.
//! - `GET …/command`: the pending command, as stored; none is a 404 that
//!   says `next_id` (what a signer signs for next) and the last result.
//! - `POST …/command/done {id, result}`: the pending one with that id is
//!   done and cleared, its result kept as text; the id already done answers
//!   again (stormbootx's "taken before"); any other is refused.

use serde::{Deserialize, Serialize};

/// stormbootx's verbs (its README, *The API, commands and the hold*).
pub const VERBS: &[&str] = &[
    "reboot", "poweroff", "boot-next", "install", "local", "retry", "hold", "inventory", "format", "maint",
];

/// The longest result kept (text: the inventory is in every claim).
pub const RESULT_MAX: usize = 16 * 1024;

/// A command as stored and answered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Command {
    pub host: String,
    pub id: u64,
    pub command: String,
    #[serde(default)]
    pub args: String,
    pub issued: String,
    #[serde(default)]
    pub by: String,
    #[serde(default)]
    pub sig: String,
}

/// What a `PUT` carries.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct CommandRequest {
    pub command: String,
    #[serde(default)]
    pub args: String,
    #[serde(default)]
    pub id: Option<u64>,
    #[serde(default)]
    pub issued: Option<String>,
    #[serde(default)]
    pub by: Option<String>,
    #[serde(default)]
    pub host: Option<String>,
    #[serde(default)]
    pub sig: Option<String>,
}

/// What a `POST …/done` carries.
#[derive(Debug, Clone, Deserialize)]
pub struct DoneRequest {
    pub id: u64,
    #[serde(default)]
    pub result: String,
}

/// The last command reported done.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Done {
    pub id: u64,
    pub command: String,
    pub result: String,
    pub at: String,
}

/// A host's command slot: the pending one, the highest id it has had and
/// the last result.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Slot {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending: Option<Command>,
    #[serde(default)]
    pub last_id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_done: Option<Done>,
}

/// Why a request is refused, and the HTTP status for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refused {
    /// 400: the request is not a command.
    Bad(String),
    /// 409: the id is not the one it may be.
    Conflict(String),
}

impl Refused {
    pub fn status(&self) -> u16 {
        match self {
            Refused::Bad(_) => 400,
            Refused::Conflict(_) => 409,
        }
    }

    pub fn message(&self) -> &str {
        match self {
            Refused::Bad(m) | Refused::Conflict(m) => m,
        }
    }
}

impl Slot {
    /// The id a command put now without one gets: what a signer signs for.
    pub fn next_id(&self) -> u64 {
        self.last_id + 1
    }

    /// Store `req` as `host`'s pending command, replacing any before it.
    /// `now` is the time to record when the request gives none (RFC 3339).
    pub fn put(&mut self, host: &str, req: CommandRequest, now: &str) -> Result<Command, Refused> {
        let verb = req.command.trim();
        if !VERBS.contains(&verb) {
            return Err(Refused::Bad(format!("command {verb:?} is none of: {}", VERBS.join(", "))));
        }
        if req.args.contains('\n') || req.args.len() > 1024 {
            return Err(Refused::Bad("args are one line of at most 1024 bytes".into()));
        }
        if let Some(h) = req.host.as_deref() {
            if h != host {
                return Err(Refused::Bad(format!(
                    "the command names host {h:?}, and this is {host:?}: a signature covers the host"
                )));
            }
        }
        let id = match req.id {
            Some(0) => return Err(Refused::Bad("an id is 1 or more".into())),
            Some(id) if id <= self.last_id => {
                return Err(Refused::Conflict(format!(
                    "id {id} is not above {}, the highest {host} has had: the next is {}",
                    self.last_id,
                    self.next_id()
                )))
            }
            Some(id) => id,
            None => self.next_id(),
        };
        let c = Command {
            host: host.to_string(),
            id,
            command: verb.to_string(),
            args: req.args,
            issued: req.issued.filter(|s| !s.trim().is_empty()).unwrap_or_else(|| now.to_string()),
            by: req.by.unwrap_or_default(),
            sig: req.sig.unwrap_or_default(),
        };
        self.last_id = id;
        self.pending = Some(c.clone());
        Ok(c)
    }

    /// The pending command `id` is done. Answers what is recorded; the one
    /// already done answers again (a repeat report), anything else is
    /// refused.
    pub fn done(&mut self, req: DoneRequest, now: &str) -> Result<Done, Refused> {
        match &self.pending {
            Some(p) if p.id == req.id => {
                let mut result = req.result;
                if result.len() > RESULT_MAX {
                    let mut cut = RESULT_MAX;
                    while !result.is_char_boundary(cut) {
                        cut -= 1;
                    }
                    result.truncate(cut);
                }
                let d = Done { id: p.id, command: p.command.clone(), result, at: now.to_string() };
                self.pending = None;
                self.last_done = Some(d.clone());
                Ok(d)
            }
            _ => match &self.last_done {
                Some(d) if d.id == req.id => Ok(d.clone()),
                _ => Err(Refused::Conflict(match &self.pending {
                    Some(p) => format!("command {} is pending, not {}", p.id, req.id),
                    None => format!("no command {} is pending", req.id),
                })),
            },
        }
    }

    /// The 404 body of a `GET` with nothing pending.
    pub fn none_pending(&self, host: &str) -> serde_json::Value {
        serde_json::json!({
            "error": format!("no command is pending for {host}"),
            "host": host,
            "next_id": self.next_id(),
            "last_done": self.last_done,
        })
    }
}

/// Now, as RFC 3339 in UTC (`2026-10-21T22:01:00Z`), for `issued` and `at`.
pub fn now_rfc3339() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    rfc3339(secs)
}

/// `secs` since the Unix epoch as RFC 3339 in UTC.
pub fn rfc3339(secs: u64) -> String {
    let (days, rem) = (secs / 86_400, secs % 86_400);
    // Civil from days (Howard Hinnant's algorithm), days since 1970-01-01.
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(cmd: &str, id: Option<u64>) -> CommandRequest {
        CommandRequest { command: cmd.into(), id, ..Default::default() }
    }

    #[test]
    fn ids_grow_and_a_used_one_is_refused() {
        let mut s = Slot::default();
        let a = s.put("server8", req("reboot", None), "t1").unwrap();
        assert_eq!((a.id, a.issued.as_str(), a.host.as_str()), (1, "t1", "server8"));
        let b = s.put("server8", req("hold", Some(17)), "t2").unwrap();
        assert_eq!(b.id, 17);
        assert_eq!(s.next_id(), 18);
        assert_eq!(s.put("server8", req("hold", Some(17)), "t3").unwrap_err().status(), 409);
        assert_eq!(s.put("server8", req("hold", Some(3)), "t3").unwrap_err().status(), 409);
        assert_eq!(s.put("server8", req("fly", None), "t3").unwrap_err().status(), 400);
        let mut other = req("reboot", None);
        other.host = Some("server1".into());
        assert_eq!(s.put("server8", other, "t3").unwrap_err().status(), 400, "signed for another host");
        assert_eq!(s.pending.as_ref().unwrap().id, 17, "a refused put changes nothing");
    }

    #[test]
    fn done_clears_the_pending_one_and_answers_a_repeat() {
        let mut s = Slot::default();
        let mut r = req("format", Some(5));
        r.args = "ZFL1ABCD fast".into();
        r.sig = Some("ab12".into());
        r.issued = Some("2026-10-21T22:01:00Z".into());
        let c = s.put("server8", r, "now").unwrap();
        assert_eq!((c.args.as_str(), c.sig.as_str(), c.issued.as_str()), ("ZFL1ABCD fast", "ab12", "2026-10-21T22:01:00Z"));
        assert_eq!(s.done(DoneRequest { id: 4, result: "x".into() }, "t").unwrap_err().status(), 409);
        let d = s.done(DoneRequest { id: 5, result: "formatted".into() }, "t").unwrap();
        assert_eq!((d.id, d.command.as_str(), d.result.as_str()), (5, "format", "formatted"));
        assert!(s.pending.is_none());
        let again = s.done(DoneRequest { id: 5, result: "taken before".into() }, "t2").unwrap();
        assert_eq!(again.result, "formatted", "a repeat answers what was recorded");
        assert_eq!(s.done(DoneRequest { id: 6, result: "".into() }, "t").unwrap_err().status(), 409);
        let body = s.none_pending("server8");
        assert_eq!((body["next_id"].as_u64(), body["last_done"]["id"].as_u64()), (Some(6), Some(5)));
    }

    #[test]
    fn times_are_rfc3339() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(1_792_620_060), "2026-10-21T22:01:00Z");
        assert_eq!(rfc3339(951_782_400), "2000-02-29T00:00:00Z");
    }

    #[test]
    fn a_long_result_is_cut_at_a_character() {
        let mut s = Slot::default();
        s.put("h", req("inventory", None), "t").unwrap();
        let d = s.done(DoneRequest { id: 1, result: "é".repeat(RESULT_MAX) }, "t").unwrap();
        assert!(d.result.len() <= RESULT_MAX && d.result.is_char_boundary(d.result.len()));
    }
}
