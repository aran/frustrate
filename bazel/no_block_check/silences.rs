//! Silences across one check target's analyses.
//!
//! A silence names what a finding names, and names differ per target: a
//! native wait is the C function that waits (`pthread_mutex_lock`), a
//! threaded-web one the function holding the wait instruction. A marked
//! function that wraps a lock has something under it natively and nothing on
//! wasip1. So no one analysis can say a silence matches nothing. Each writes
//! the silences it applied (`--applied`), and `--silences` reads them all and
//! fails on a silence that none applied.

use std::collections::BTreeSet;

use super::Claims;

/// The first line of an applied-silences file, and the only version read.
const HEADER: &str = "frustrate-silences 1";

/// One silence, as an analysis applies it.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Silence {
    /// `no_block(allow(name))` on the claim with this fn id: `(id, name)`.
    Member(String, String),
    /// A name in the check target's `allow`.
    Target(String),
    /// A function marked `#[frustrate::no_block(allow)]`, by path.
    Mark(String),
}

/// What one analysis did with the silences it saw.
#[derive(Default, Debug)]
pub(crate) struct Applied {
    /// Each silence that kept a finding out of the verdict.
    pub(crate) silences: BTreeSet<Silence>,
    /// Marked functions a claimed member reaches with nothing under them that
    /// would be a finding here. A marked function no claim reaches is in
    /// neither set: it is ignored.
    pub(crate) idle_marks: BTreeSet<String>,
}

impl Applied {
    /// The file `--applied` writes, for `--silences` to read.
    pub(crate) fn text(&self, label: &str) -> String {
        let mut out = format!("{HEADER}\ncheck {label}\n");
        for s in &self.silences {
            out.push_str(&match s {
                Silence::Member(id, w) => format!("member {id} {w}\n"),
                Silence::Target(w) => format!("target {w}\n"),
                Silence::Mark(p) => format!("mark {p}\n"),
            });
        }
        for p in &self.idle_marks {
            out.push_str(&format!("idle-mark {p}\n"));
        }
        out
    }

    /// Read a file [`Applied::text`] wrote: the analysis's label, and what it
    /// applied.
    pub(crate) fn parse(text: &str, path: &str) -> Result<(String, Applied), String> {
        let mut lines = text.lines();
        if lines.next() != Some(HEADER) {
            return Err(format!(
                "no_block_check: {path} is not an applied-silences file this tool \
                 reads; expected `{HEADER}` first."
            ));
        }
        let mut label = None;
        let mut a = Applied::default();
        for line in lines.filter(|l| !l.is_empty()) {
            let (kind, rest) = line.split_once(' ').unwrap_or((line, ""));
            match kind {
                "check" => label = Some(rest.to_string()),
                "target" => {
                    a.silences.insert(Silence::Target(rest.to_string()));
                }
                "mark" => {
                    a.silences.insert(Silence::Mark(rest.to_string()));
                }
                "idle-mark" => {
                    a.idle_marks.insert(rest.to_string());
                }
                "member" if rest.contains(' ') => {
                    let (id, w) = rest.split_once(' ').unwrap();
                    a.silences.insert(Silence::Member(id.to_string(), w.to_string()));
                }
                _ => return Err(format!("no_block_check: {path}: cannot read `{line}`.")),
            }
        }
        let label = label.ok_or_else(|| format!("no_block_check: {path} names no check."))?;
        Ok((label, a))
    }
}

/// Every silence the check target declares, against what its analyses
/// applied: green lists which analysis applied each, red names each that none
/// did.
pub(crate) fn judge(
    label: &str,
    claims: &Claims,
    allowed: &[String],
    checks: &[(String, Applied)],
) -> Result<String, String> {
    let by = |s: &Silence| -> Vec<&str> {
        checks
            .iter()
            .filter(|(_, a)| a.silences.contains(s))
            .map(|(l, _)| l.as_str())
            .collect()
    };
    let rooted: BTreeSet<&String> = claims.roots().filter_map(|(s, _)| claims.ids.get(s)).collect();
    let mut applied: Vec<String> = Vec::new();
    let mut stale: Vec<String> = Vec::new();
    let mut note = |s: &Silence, what: String, why: String| {
        let who = by(s);
        if who.is_empty() {
            stale.push(format!("  {what}: {why}"));
        } else {
            applied.push(format!("  {what}, applied by {}", who.join(", ")));
        }
    };

    for (id, names) in &claims.allow {
        let who = claims.display_of(id);
        for w in names {
            let why = if rooted.contains(id) {
                "no analysis found anything by that name in its closure".to_string()
            } else {
                "the claim is settled without a root, so nothing it reaches is scanned".to_string()
            };
            note(&Silence::Member(id.clone(), w.clone()), format!("`{w}` on {who}"), why);
        }
    }
    for w in allowed {
        note(
            &Silence::Target(w.clone()),
            format!("`{w}` in the check target's `allow`"),
            "no analysis found anything by that name in a claim or a bounded-lock section"
                .to_string(),
        );
    }
    // Marks are known only from the builds: each one some analysis saw a
    // claimed member reach.
    let mut marks: BTreeSet<&str> = BTreeSet::new();
    for (_, a) in checks {
        marks.extend(a.idle_marks.iter().map(String::as_str));
        for s in &a.silences {
            if let Silence::Mark(p) = s {
                marks.insert(p);
            }
        }
    }
    for p in marks {
        note(
            &Silence::Mark(p.to_string()),
            format!("`{p}` (`#[frustrate::no_block(allow)]`)"),
            "a claimed member reaches it, but no analysis found anything under it".to_string(),
        );
    }

    let names: Vec<&str> = checks.iter().map(|(l, _)| l.as_str()).collect();
    if !stale.is_empty() {
        return Err(format!(
            "no_block_check: {label}: {} silence(s) silence nothing on any of \
             {}:\n\n{}\n\n\
             A silence that matches nothing would hide whatever later comes to \
             match it, so it is an error rather than a no-op. Remove it, or \
             correct what it names to what a finding says to silence it as (for \
             an attribute, remove `#[frustrate::no_block(allow)]` from the \
             function).",
            stale.len(),
            names.join(", "),
            stale.join("\n")
        ));
    }
    if applied.is_empty() {
        return Ok(format!(
            "no_block_check: {label}: no silences across {}.",
            names.join(", ")
        ));
    }
    Ok(format!(
        "no_block_check: {label}: every silence silences something:\n{}",
        applied.join("\n")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn census(rows: &str) -> Claims {
        Claims::parse(
            &format!("{}\ncrate example\n{rows}", super::super::CENSUS_HEADER),
            "c",
        )
        .unwrap()
    }

    fn applied(label: &str, silences: &[Silence], idle: &[&str]) -> (String, Applied) {
        (
            label.to_string(),
            Applied {
                silences: silences.iter().cloned().collect(),
                idle_marks: idle.iter().map(|p| p.to_string()).collect(),
            },
        )
    }

    #[test]
    fn the_file_round_trips() {
        let (_, a) = applied(
            "x",
            &[
                Silence::Member("3".into(), "std::sys::futex_wait".into()),
                Silence::Target("sqlite3_step".into()),
                Silence::Mark("example::wrap".into()),
            ],
            &["example::idle"],
        );
        let (label, back) = Applied::parse(&a.text("//p:c.native"), "f").unwrap();
        assert_eq!(label, "//p:c.native");
        assert_eq!(back.silences, a.silences);
        assert_eq!(back.idle_marks, a.idle_marks);
    }

    /// A silence one analysis applies is used, whatever the others found.
    #[test]
    fn a_silence_applied_by_one_analysis_is_used() {
        let c = census("artifact 1 add crate::add\nallow 1 pthread_mutex_lock\nallow 1 std::futex_wait\n");
        let ok = judge(
            "//p:c",
            &c,
            &[],
            &[
                applied("//p:c.native", &[Silence::Member("1".into(), "pthread_mutex_lock".into())], &[]),
                applied("//p:c.web_threads", &[Silence::Member("1".into(), "std::futex_wait".into())], &[]),
            ],
        )
        .unwrap();
        assert!(ok.contains("`std::futex_wait` on add, applied by //p:c.web_threads"), "{ok}");
        assert!(ok.contains("`pthread_mutex_lock` on add, applied by //p:c.native"), "{ok}");
    }

    #[test]
    fn a_silence_no_analysis_applies_is_an_error() {
        let c = census("artifact 1 add crate::add\nallow 1 sqlite3_step\nplacement 2 put\nallow 2 nanosleep\n");
        let e = judge(
            "//p:c",
            &c,
            &["getaddrinfo".to_string()],
            &[applied("//p:c.native", &[], &[]), applied("//p:c.wasi", &[], &[])],
        )
        .unwrap_err();
        assert!(e.contains("3 silence(s) silence nothing on any of //p:c.native, //p:c.wasi"), "{e}");
        assert!(e.contains("`sqlite3_step` on add: no analysis found"), "{e}");
        assert!(e.contains("`nanosleep` on put: the claim is settled without a root"), "{e}");
        assert!(e.contains("`getaddrinfo` in the check target's `allow`"), "{e}");
    }

    /// Both of two linked bridges number a member 1 and silence it. Applied
    /// for one bridge, the other's silence is still unused, and the message
    /// says whose.
    #[test]
    fn linked_bridges_are_judged_each_on_its_own_silences() {
        let bridge = |krate: &str| {
            Claims::parse(
                &format!(
                    "{}\ncrate {krate}\nartifact 1 add crate::add\nallow 1 pthread_mutex_lock\n",
                    super::super::CENSUS_HEADER
                ),
                krate,
            )
            .unwrap()
        };
        let c = Claims::merge(vec![("example".into(), bridge("example")), ("app".into(), bridge("app"))]).unwrap();
        let e = judge(
            "//p:c",
            &c,
            &[],
            &[applied("//p:c.native", &[Silence::Member("example:1".into(), "pthread_mutex_lock".into())], &[])],
        )
        .unwrap_err();
        assert!(e.contains("1 silence(s) silence nothing"), "{e}");
        assert!(e.contains("`pthread_mutex_lock` on app: add: no analysis found"), "{e}");
        assert!(!e.contains("on example: add"), "{e}");
        let ok = judge(
            "//p:c",
            &c,
            &[],
            &[applied(
                "//p:c.native",
                &[
                    Silence::Member("example:1".into(), "pthread_mutex_lock".into()),
                    Silence::Member("app:1".into(), "pthread_mutex_lock".into()),
                ],
                &[],
            )],
        )
        .unwrap();
        assert!(ok.contains("on example: add, applied by //p:c.native"), "{ok}");
        assert!(ok.contains("on app: add, applied by //p:c.native"), "{ok}");
    }

    /// A marked function idle on one analysis and applied on another is used;
    /// idle on every one, it is an error.
    #[test]
    fn a_marked_function_is_judged_across_analyses() {
        let c = census("artifact 1 add crate::add\n");
        let mark = Silence::Mark("example::wrap".into());
        let ok = judge(
            "//p:c",
            &c,
            &[],
            &[
                applied("//p:c.native", std::slice::from_ref(&mark), &[]),
                applied("//p:c.wasi", &[], &["example::wrap"]),
            ],
        )
        .unwrap();
        assert!(ok.contains("`example::wrap` (`#[frustrate::no_block(allow)]`), applied by //p:c.native"), "{ok}");
        let e = judge("//p:c", &c, &[], &[applied("//p:c.wasi", &[], &["example::wrap"])]).unwrap_err();
        assert!(e.contains("`example::wrap` (`#[frustrate::no_block(allow)]`): a claimed member reaches it"), "{e}");
    }
}
