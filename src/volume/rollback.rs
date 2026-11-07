//! Rollback copies (#565): the node's volumes an install or upgrade set aside.
//!
//! An install over a node that keeps its data half renames a node volume
//! aside when the release replaces it (a golden of the release keeps its
//! name, a `replace` or `migrate` policy): `<name>@<previous release>`, or
//! with `.2`, `.3`… when that name is taken. That copy is what rolling back
//! once needs. Nothing deleted them, so every install added one more per
//! name (forge1: 27 data templates × 5 generations).
//!
//! Since #565 each copy carries a [`RollbackMark`] (what it was, from which
//! release, its generation), written in the record. A copy set aside before
//! that is recognised by its name: `<of>@<label>[.N]` beside a live `<of>` in
//! the same namespace. Volumes of a #122 stage (`<name>@<version>` while
//! staged, the generation kept for a rollback) have their own bookkeeping
//! and are never counted here: the caller passes their ids.
//!
//! The prune keeps the newest `keep` copies of each name and deletes the
//! rest. Order is the generation; copies recognised by name only are older
//! than any marked one, ordered among themselves by their names compared
//! number by number (`@previous` < `@previous.2` < … < `@previous.10`).

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

use super::metadata::RollbackMark;
use super::{VolumeId, VolumeManager};

/// One rollback copy on the node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Copy {
    pub id: VolumeId,
    pub name: String,
    pub namespace: String,
    pub mark: RollbackMark,
    /// Recognised by its name, with no mark written yet (#565).
    pub by_name: bool,
}

/// `<of>@<label>`: split at the last `@`, both sides non-empty.
pub fn split(name: &str) -> Option<(&str, &str)> {
    let (of, label) = name.rsplit_once('@')?;
    (!of.is_empty() && !label.is_empty()).then_some((of, label))
}

/// The release a copy's label names: `previous.3` → `previous`, `12.60` stays.
/// A `.N` suffix is only an install's when the label without it is not itself
/// a version number, so `12.60` is not read as `12` copy 60.
fn label_release(label: &str) -> &str {
    if let Some((head, n)) = label.rsplit_once('.') {
        let numeric_head = head.split('.').all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()));
        if !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) && !numeric_head {
            return head;
        }
    }
    label
}

/// Names compared with runs of digits as numbers.
pub fn natural_cmp(a: &str, b: &str) -> Ordering {
    let (mut a, mut b) = (a.as_bytes(), b.as_bytes());
    loop {
        match (a.first(), b.first()) {
            (None, None) => return Ordering::Equal,
            (None, _) => return Ordering::Less,
            (_, None) => return Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let da = a.iter().take_while(|c| c.is_ascii_digit()).count();
                let db = b.iter().take_while(|c| c.is_ascii_digit()).count();
                let (na, nb) = (&a[..da], &b[..db]);
                let strip = |s: &[u8]| -> Vec<u8> { s.iter().copied().skip_while(|c| *c == b'0').collect() };
                let (sa, sb) = (strip(na), strip(nb));
                let o = sa.len().cmp(&sb.len()).then_with(|| sa.cmp(&sb));
                if o != Ordering::Equal {
                    return o;
                }
                a = &a[da..];
                b = &b[db..];
            }
            (Some(x), Some(y)) => {
                if x != y {
                    return x.cmp(y);
                }
                a = &a[1..];
                b = &b[1..];
            }
        }
    }
}

/// What every volume is, as the recognition needs it.
#[derive(Debug, Clone)]
pub struct Seen {
    pub id: VolumeId,
    pub name: String,
    pub namespace: String,
    pub mark: Option<RollbackMark>,
}

/// The rollback copies among `volumes`, leaving out `exclude` (a #122
/// stage's volumes). A copy recognised by its name gets a mark whose
/// generation is its rank among the name-only copies of the same name.
pub fn recognise(volumes: &[Seen], exclude: &HashSet<VolumeId>) -> Vec<Copy> {
    let live: HashSet<(&str, &str)> = volumes.iter().map(|v| (v.namespace.as_str(), v.name.as_str())).collect();
    let mut out = Vec::new();
    let mut by_name: HashMap<(String, String), Vec<&Seen>> = HashMap::new();
    for v in volumes.iter().filter(|v| !exclude.contains(&v.id)) {
        if let Some(m) = &v.mark {
            out.push(Copy { id: v.id, name: v.name.clone(), namespace: v.namespace.clone(), mark: m.clone(), by_name: false });
            continue;
        }
        let Some((of, _)) = split(&v.name) else { continue };
        if live.contains(&(v.namespace.as_str(), of)) {
            by_name.entry((v.namespace.clone(), of.to_string())).or_default().push(v);
        }
    }
    for ((namespace, of), mut vs) in by_name {
        vs.sort_by(|a, b| natural_cmp(&a.name, &b.name));
        for (i, v) in vs.into_iter().enumerate() {
            let label = split(&v.name).map(|(_, l)| l).unwrap_or_default();
            out.push(Copy {
                id: v.id,
                name: v.name.clone(),
                namespace: namespace.clone(),
                mark: RollbackMark {
                    of: of.clone(),
                    from: label_release(label).to_string(),
                    generation: i as u32 + 1,
                    set_aside_at: 0,
                },
                by_name: true,
            });
        }
    }
    out.sort_by(order);
    out
}

/// Oldest first: name-only copies before marked ones, then the generation,
/// then the name.
fn order(a: &Copy, b: &Copy) -> Ordering {
    (a.namespace.as_str(), a.mark.of.as_str())
        .cmp(&(b.namespace.as_str(), b.mark.of.as_str()))
        .then_with(|| b.by_name.cmp(&a.by_name))
        .then_with(|| a.mark.generation.cmp(&b.mark.generation))
        .then_with(|| natural_cmp(&a.name, &b.name))
}

/// Of each name's copies, the newest `keep` stay; the rest go. Returns
/// `(kept, to_prune)`, each oldest first.
pub fn plan(copies: &[Copy], keep: usize) -> (Vec<Copy>, Vec<Copy>) {
    let mut groups: HashMap<(&str, &str), Vec<&Copy>> = HashMap::new();
    for c in copies {
        groups.entry((c.namespace.as_str(), c.mark.of.as_str())).or_default().push(c);
    }
    let (mut kept, mut gone) = (Vec::new(), Vec::new());
    for (_, mut g) in groups {
        g.sort_by(order);
        let cut = g.len().saturating_sub(keep);
        gone.extend(g[..cut].iter().map(|c| (*c).clone()));
        kept.extend(g[cut..].iter().map(|c| (*c).clone()));
    }
    kept.sort_by(order);
    gone.sort_by(order);
    (kept, gone)
}

/// What the manager holds, for [`recognise`].
pub async fn seen(mgr: &VolumeManager) -> Vec<Seen> {
    let mut out = Vec::new();
    for (id, name, _, _) in mgr.list_volumes().await {
        out.push(Seen {
            namespace: mgr.namespace(&id).to_string(),
            mark: mgr.rollback(&id).cloned(),
            id,
            name,
        });
    }
    out
}

/// The generation the next copy of `of` takes: one past the newest there is.
pub async fn next_generation(mgr: &VolumeManager, namespace: &str, of: &str) -> u32 {
    let copies = recognise(&seen(mgr).await, &HashSet::new());
    let marked = copies
        .iter()
        .filter(|c| c.namespace == namespace && c.mark.of == of && !c.by_name)
        .map(|c| c.mark.generation)
        .max()
        .unwrap_or(0);
    let named = copies.iter().filter(|c| c.namespace == namespace && c.mark.of == of && c.by_name).count() as u32;
    marked.max(named) + 1
}

/// Write the marks of copies recognised only by their name (#565), so the
/// API calls them rollback copies. Returns how many were marked; the caller
/// persists.
pub fn mark_by_name(mgr: &mut VolumeManager, copies: &[Copy]) -> usize {
    let mut n = 0;
    for c in copies.iter().filter(|c| c.by_name) {
        if mgr.set_rollback(c.id, Some(c.mark.clone())).is_ok() {
            n += 1;
        }
    }
    n
}

/// Unix seconds now.
pub fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(name: &str, mark: Option<(u32, &str, &str)>) -> Seen {
        Seen {
            id: VolumeId(uuid::Uuid::new_v4()),
            name: name.into(),
            namespace: "stormcos".into(),
            mark: mark.map(|(g, of, from)| RollbackMark { of: of.into(), from: from.into(), generation: g, set_aside_at: 1 }),
        }
    }

    #[test]
    fn names_compare_number_by_number() {
        let mut n = vec!["x@previous.10", "x@previous.2", "x@previous", "x@previous.3"];
        n.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(n, ["x@previous", "x@previous.2", "x@previous.3", "x@previous.10"]);
        assert_eq!(natural_cmp("x@12.60", "x@12.61"), Ordering::Less);
        assert_eq!(natural_cmp("x@12.9", "x@12.10"), Ordering::Less);
        assert_eq!(label_release("previous.3"), "previous");
        assert_eq!(label_release("12.60"), "12.60");
        // A version's own dots are never a copy number: `from` is said as given.
        assert_eq!(label_release("12.60.2"), "12.60.2");
    }

    /// forge1: five generations of a template by name, none marked; the live
    /// template and a volume with an `@` of its own (no live base) are not
    /// copies, and a staged volume is never one.
    #[test]
    fn forge1s_pile_is_recognised_and_pruned_to_one() {
        let mut vols = vec![v("cadvisor-data.golden", None), v("fastetcd-data.golden", None), v("odd@name", None)];
        for s in ["", ".2", ".3", ".4", ".5"] {
            vols.push(v(&format!("cadvisor-data.golden@previous{s}"), None));
            vols.push(v(&format!("fastetcd-data.golden@previous{s}"), None));
        }
        let staged = v("cadvisor-data.golden@12.70", None);
        let exclude: HashSet<VolumeId> = [staged.id].into();
        vols.push(staged);
        let copies = recognise(&vols, &exclude);
        assert_eq!(copies.len(), 10, "{copies:#?}");
        assert!(copies.iter().all(|c| c.by_name && c.mark.from == "previous"));
        let (kept, gone) = plan(&copies, 1);
        let mut kept: Vec<&str> = kept.iter().map(|c| c.name.as_str()).collect();
        kept.sort();
        assert_eq!(kept, ["cadvisor-data.golden@previous.5", "fastetcd-data.golden@previous.5"]);
        assert_eq!(gone.len(), 8);
        let (k0, g0) = plan(&copies, 0);
        assert!(k0.is_empty() && g0.len() == 10);
        let (k9, g9) = plan(&copies, 9);
        assert!(g9.is_empty() && k9.len() == 10);
    }

    /// A marked copy is newer than any recognised by name, whatever the names
    /// say: once `@previous` is pruned, the next install may take that name
    /// again.
    #[test]
    fn a_marked_copy_outranks_the_names() {
        let vols = vec![
            v("a", None),
            v("a@previous.4", None),
            v("a@previous", Some((6, "a", "previous"))),
            v("a@previous.5", Some((5, "a", "previous"))),
        ];
        let copies = recognise(&vols, &HashSet::new());
        let (kept, gone) = plan(&copies, 1);
        assert_eq!(kept.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["a@previous"]);
        assert_eq!(gone.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["a@previous.4", "a@previous.5"]);
    }
}
