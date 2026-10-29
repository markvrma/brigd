//! An agent's diffs: its base commit (recorded at launch, read back here), the files it
//! changed per its transcripts and output.md, their `git diff` vs that base (cached, then
//! frozen to <agent>/.diff/ once it is done), and reverting them.

use crate::transcript::{add, changed_files, subagents_dir, transcript_path};
use crate::Res;
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How long a cached git listing is trusted before git is asked again.
const GIT_RECHECK: Duration = Duration::from_secs(3);
/// How long after output.md is written the transcript counts as settled and the diffs get sealed.
const SEAL_AFTER: Duration = Duration::from_secs(10);

/// Transcript path → (bytes parsed, up to the last full line; files it changed so far).
static PARSED: Mutex<BTreeMap<PathBuf, (u64, Vec<PathBuf>)>> = Mutex::new(BTreeMap::new());

/// A base commit and the files `git diff --numstat` lists vs it, with their (added, removed)
/// line counts, "-" for binary.
type Listing = (String, HashMap<PathBuf, (String, String)>);
/// Agent dir → (when checked; its listing, None when git failed).
type Names = (Instant, Option<Listing>);
static NAMES: Mutex<BTreeMap<PathBuf, Names>> = Mutex::new(BTreeMap::new());

/// Diff node path → (agent cwd, base commit, file path relative to the cwd, (added, removed)).
type View = (PathBuf, String, String, (String, String));
static VIEWS: Mutex<BTreeMap<PathBuf, View>> = Mutex::new(BTreeMap::new());

#[cfg(test)]
thread_local!(static GIT_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) });

fn git(cwd: &Path, args: &[&str]) -> Option<String> {
    #[cfg(test)]
    GIT_CALLS.with(|c| c.set(c.get() + 1));
    let o = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    Some(String::from_utf8_lossy(&o.stdout).into_owned())
}

/// The commit an agent started from, for its diffs; a relaunch keeps the first one.
pub(crate) fn record_base(cwd: &Path, dir: &Path) {
    if !dir.join("base").exists() {
        if let Some(o) = Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(["rev-parse", "HEAD"])
            .output()
            .ok()
            .filter(|o| o.status.success())
        {
            let _ = fs::write(dir.join("base"), &o.stdout);
        }
    }
}

/// The commit an agent started from: <dir>/base (written at launch), else where its
/// cwd's HEAD forked from the repo's HEAD. If HEAD moved on with commits made before launch
/// (a `git pull`), the newest of those becomes the base, so pulled changes aren't the agent's.
fn base(cwd: &Path, repo: &Path, dir: &Path) -> Option<String> {
    let file = dir.join("base");
    if let Ok(b) = fs::read_to_string(&file) {
        let b = b.trim().to_string();
        let launch = fs::metadata(&file)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok());
        let moved = git(cwd, &["rev-parse", "HEAD"]).is_some_and(|h| h.trim() != b);
        if let (true, Some(launch)) = (moved, launch) {
            // strictly before the launch second: a commit in that second counts as the agent's
            let before = format!("--before={}", launch.as_secs().saturating_sub(1));
            if let Some(c) = git(cwd, &["rev-list", "-1", &before, "HEAD"]) {
                let c = c.trim();
                if !c.is_empty() && git(cwd, &["merge-base", "--is-ancestor", &b, c]).is_some() {
                    return Some(c.to_string());
                }
            }
        }
        return Some(b);
    }
    let head = git(repo, &["rev-parse", "HEAD"])?;
    Some(
        git(cwd, &["merge-base", "HEAD", head.trim()])?
            .trim()
            .to_string(),
    )
}

/// The files the agent's session (and its subagents) changed, per their transcripts, in
/// first-change order. Parsed incrementally: only bytes appended since the last call.
fn transcript_files(home: &Path, cwd: &Path, dir: &Path) -> Vec<PathBuf> {
    let Ok(session) = fs::read_to_string(dir.join("session")) else {
        return vec![];
    };
    let session = session.trim();
    let mut logs: Vec<PathBuf> = fs::read_dir(subagents_dir(home, cwd, session))
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "jsonl"))
        .collect();
    logs.sort();
    logs.insert(0, transcript_path(home, cwd, session));
    let mut files: Vec<PathBuf> = Vec::new();
    let mut parsed = PARSED.lock().unwrap();
    for log in logs {
        let Ok(mut f) = fs::File::open(&log) else {
            continue;
        };
        let (done, seen) = parsed.entry(log).or_default();
        let len = f.metadata().map(|m| m.len()).unwrap_or(0);
        if len < *done {
            (*done, *seen) = (0, vec![]); // rewritten: start over
        }
        let mut tail = Vec::new();
        if len > *done && f.seek(SeekFrom::Start(*done)).is_ok() && f.read_to_end(&mut tail).is_ok()
        {
            let end = tail.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
            add(seen, changed_files(&String::from_utf8_lossy(&tail[..end])));
            *done += end as u64;
        }
        add(&mut files, seen.clone());
    }
    files
}

/// The paths under `## Files changed` (`- /abs/path` lines) in the agent's own output.md.
fn reported_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(text) = fs::read_to_string(dir.join("output.md")) else {
        return vec![];
    };
    let mut lines = text
        .lines()
        .skip_while(|l| l.trim() != "## Files changed")
        .skip(1)
        .take_while(|l| !l.starts_with('#'));
    lines
        .by_ref()
        .filter_map(|l| l.trim().strip_prefix("- "))
        .map(|p| p.trim().trim_matches('`'))
        .filter(|p| p.starts_with('/'))
        .map(PathBuf::from)
        .collect()
}

/// What the agent changed: its transcripts' files plus the ones it listed in its output.md.
fn agent_files(home: &Path, cwd: &Path, dir: &Path) -> Vec<PathBuf> {
    let mut files = transcript_files(home, cwd, dir);
    add(&mut files, reported_files(dir));
    files
}

/// The diff view path (<dir>/.diff/<file>, read by diff_view) of each file the agent's session
/// (and its subagents) changed, inside its cwd and outside its brigd dir, that `git diff` vs its
/// base still shows (so new untracked files don't). Git is asked at most every 3s per agent.
/// Each path keeps its file's name, which the sidebar shows.
pub(crate) fn diff_paths(home: &Path, cwd: &Path, repo: &Path, dir: &Path) -> Vec<PathBuf> {
    let files = agent_files(home, cwd, dir);
    if files.is_empty() {
        return vec![];
    }
    // Sealed: the agent finished and its diffs are frozen, so git has nothing new to say.
    let sealed = dir.join(".diff/.sealed");
    let (base, listed) = if sealed.exists() {
        Default::default()
    } else {
        refresh_listing(cwd, repo, dir, &sealed)
    };
    // Once the agent is done (output.md written) its diffs are saved and never recomputed.
    let done = dir.join("output.md").exists();
    let mut views = VIEWS.lock().unwrap();
    files
        .into_iter()
        .filter(|f| !f.starts_with(dir)) // its own output.md already shows
        .filter_map(|file| {
            let path = dir
                .join(".diff")
                .join(file.strip_prefix("/").unwrap_or(&file));
            file.file_name()?;
            if path.is_file() {
                return Some(path);
            }
            let stat = listed.get(&file)?.clone();
            let rel = file.strip_prefix(cwd).ok()?.to_string_lossy().into_owned();
            if done {
                let text = git(cwd, &["diff", "--no-color", &base, "--", &rel]);
                let save = |t: String| {
                    fs::create_dir_all(path.parent()?).ok().and_then(|_| {
                        fs::write(&path, format!("{}\t{}\t{rel}\n{t}", stat.0, stat.1)).ok()
                    })
                };
                if text.and_then(save).is_some() {
                    return Some(path);
                }
            }
            views.insert(path.clone(), (cwd.into(), base.clone(), rel, stat));
            Some(path)
        })
        .collect()
}

/// The agent's base commit and the files `git diff --numstat` lists vs it, from NAMES, asking
/// git again at most every 3s; seals the agent once it is done and settled. Empty when git
/// failed. Holds the NAMES lock across the git calls.
fn refresh_listing(cwd: &Path, repo: &Path, dir: &Path, sealed: &Path) -> Listing {
    let mut names = NAMES.lock().unwrap();
    let entry = names.entry(dir.into()).or_insert((Instant::now(), None));
    if entry.1.is_none() || entry.0.elapsed() >= GIT_RECHECK {
        // first check: backdate by a stable per-dir 0–3s phase so agents' rechecks spread out
        let mut h = DefaultHasher::new();
        dir.hash(&mut h);
        let phase = Duration::from_millis(if entry.1.is_none() {
            h.finish() % GIT_RECHECK.as_millis() as u64
        } else {
            0
        });
        let b = base(cwd, repo, dir);
        let listed = b.as_ref().and_then(|b| {
            git(
                cwd,
                &["diff", "--numstat", "--no-renames", "--relative", "-z", b],
            )
        });
        *entry = (
            Instant::now()
                .checked_sub(phase)
                .unwrap_or_else(Instant::now),
            b.zip(listed).map(|(b, l)| (b, numstat(&l, cwd))),
        );
        // done ≥10s ago (transcript settled) after a good git pass, or cwd gone: seal
        let old = fs::metadata(dir.join("output.md"))
            .and_then(|m| m.modified())
            .is_ok_and(|t| t.elapsed().is_ok_and(|a| a >= SEAL_AFTER));
        if (entry.1.is_some() && old) || !cwd.exists() {
            let _ = fs::create_dir_all(dir.join(".diff")).and_then(|_| fs::write(sealed, ""));
        }
    }
    // no base/listing (git failed): only the already frozen diffs can show
    entry.1.clone().unwrap_or_default()
}

/// `git diff --numstat -z --no-renames` → path → (added, removed).
fn numstat(out: &str, cwd: &Path) -> HashMap<PathBuf, (String, String)> {
    out.split('\0')
        .filter_map(|l| {
            let mut f = l.splitn(3, '\t');
            let (a, d, n) = (f.next()?, f.next()?, f.next()?);
            Some((cwd.join(n), (a.into(), d.into())))
        })
        .collect()
}

/// What a Diff node shows: (path relative to the agent's cwd, `git diff` of it vs the agent's base).
pub(crate) fn diff_view(key: &Path) -> Option<(String, String)> {
    if let Some((_, rel, text)) = frozen(key) {
        return Some((rel, text));
    }
    let (cwd, base, rel, _) = VIEWS.lock().unwrap().get(key).cloned()?;
    let text = git(&cwd, &["diff", "--no-color", &base, "--", &rel]).unwrap_or_default();
    Some((rel, text))
}

/// A Diff node's (added, removed) line counts vs the agent's base.
pub(crate) fn diff_stat(key: &Path) -> Option<(String, String)> {
    frozen(key)
        .map(|f| f.0)
        .or_else(|| VIEWS.lock().unwrap().get(key).map(|v| v.3.clone()))
}

/// A finished agent's diff of one file, saved at the Diff node's path so that later changes
/// to the file (by other agents) never alter it: "<added>\t<removed>\t<rel path>\n<patch>".
fn frozen(key: &Path) -> Option<((String, String), String, String)> {
    let s = fs::read_to_string(key).ok()?;
    let (head, text) = s.split_once('\n')?;
    let mut h = head.splitn(3, '\t');
    Some((
        (h.next()?.into(), h.next()?.into()),
        h.next()?.into(),
        text.into(),
    ))
}

/// Which of the agent's changed files `revert` may touch, as paths relative to `cwd`:
/// only inside the cwd (no `..`) and never its own brigd dir (output.md stays).
fn revert_set(files: &[PathBuf], cwd: &Path, dir: &Path) -> Vec<String> {
    files
        .iter()
        .filter(|f| !f.starts_with(dir))
        .filter_map(|f| f.strip_prefix(cwd).ok())
        .filter(|r| {
            !r.as_os_str().is_empty() && r.components().all(|c| matches!(c, Component::Normal(_)))
        })
        .map(|r| r.to_string_lossy().into_owned())
        .collect()
}

/// Puts back what the agent changed: each file its transcripts name (and only those, so
/// other agents' files in a shared worktree are safe) goes back to its content at the
/// agent's base commit, or is deleted if the base had no such file. Never touches the
/// agent's own dir. Returns how many files differed and were reverted.
pub(crate) fn revert(home: &Path, cwd: &Path, repo: &Path, dir: &Path) -> Res<usize> {
    let base = base(cwd, repo, dir).ok_or("no base commit for this agent")?;
    let mut n = 0;
    for rel in revert_set(&agent_files(home, cwd, dir), cwd, dir) {
        let in_base = git(cwd, &["cat-file", "-e", &format!("{base}:./{rel}")]).is_some();
        let abs = cwd.join(&rel);
        if in_base {
            if git(cwd, &["diff", "--name-only", &base, "--", &rel])
                .is_some_and(|d| !d.trim().is_empty())
            {
                git(cwd, &["checkout", &base, "--", &rel])
                    .ok_or_else(|| format!("git checkout {rel} failed"))?;
                n += 1;
            }
        } else if abs.is_file() {
            git(
                cwd,
                &["rm", "-q", "--cached", "--ignore-unmatch", "--", &rel],
            );
            fs::remove_file(&abs).map_err(|e| format!("{rel}: {e}"))?;
            n += 1;
        }
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transcript::slug;
    use crate::tree::tests::tmp;

    /// What the sidebar labels a diff path with.
    fn name(p: &Path) -> &str {
        p.file_name().unwrap().to_str().unwrap()
    }

    #[test]
    fn diff_paths_from_git() {
        let d = tmp("diffs");
        let (home, cwd, dir) = (d.join("home"), d.join("repo"), d.join("agent"));
        fs::create_dir_all(&dir).unwrap();
        fs::create_dir_all(cwd.join("src")).unwrap();
        let g = |a: &[&str]| {
            assert!(Command::new("git")
                .arg("-C")
                .arg(&cwd)
                .args(["-c", "user.name=t", "-c", "user.email=t@t"])
                .args(a)
                .output()
                .unwrap()
                .status
                .success())
        };
        g(&["init", "-q"]);
        fs::write(cwd.join("src/a.rs"), "one\n").unwrap();
        g(&["add", "."]);
        g(&["commit", "-qm", "init"]);
        fs::write(dir.join("base"), git(&cwd, &["rev-parse", "HEAD"]).unwrap()).unwrap();
        fs::write(cwd.join("src/a.rs"), "one\ntwo\n").unwrap();
        g(&["commit", "-qam", "agent"]);
        fs::write(cwd.join("src/a.rs"), "one\ntwo\nthree\n").unwrap();
        fs::write(cwd.join("new.rs"), "x\n").unwrap(); // untracked
        fs::write(dir.join("session"), "s1\n").unwrap();
        let log = home
            .join(".claude/projects")
            .join(slug(&cwd))
            .join("s1.jsonl");
        fs::create_dir_all(log.parent().unwrap()).unwrap();
        let edit =
            |f: &Path| serde_json::json!({"toolUseResult": {"filePath": f}}).to_string() + "\n";
        let (a, new) = (cwd.join("src/a.rs"), cwd.join("new.rs"));
        fs::write(&log, edit(&new) + &edit(&a)[..20]).unwrap(); // second line still being written
        assert!(diff_paths(&home, &cwd, &cwd, &dir).is_empty());
        fs::write(&log, edit(&new) + &edit(&a) + &edit(&dir.join("output.md"))).unwrap();
        let n = diff_paths(&home, &cwd, &cwd, &dir);
        assert_eq!((n.len(), name(&n[0])), (1, "a.rs"));
        let (rel, text) = diff_view(&n[0]).unwrap();
        assert_eq!(rel, "src/a.rs");
        assert!(
            text.contains("+two\n") && text.contains("+three\n"),
            "{text}"
        );
        assert_eq!(diff_stat(&n[0]), Some(("2".into(), "0".into())));
        // output.md lists a file the transcript never named (a Bash edit); the agent is done: diffs freeze
        fs::write(cwd.join("b.rs"), "b\n").unwrap();
        g(&["add", "b.rs"]);
        fs::write(
            dir.join("output.md"),
            format!(
                "done\n## Files changed\n- {}\n- `{}`\n- rel/ignored\n\n## Other\n- /not/listed\n",
                a.display(),
                cwd.join("b.rs").display()
            ),
        )
        .unwrap();
        NAMES.lock().unwrap().clear();
        let n = diff_paths(&home, &cwd, &cwd, &dir);
        assert_eq!(
            n.iter().map(|x| name(x)).collect::<Vec<_>>(),
            ["a.rs", "b.rs"]
        );
        let before = diff_view(&n[0]).unwrap();
        // a later agent changes the file again, and the 3s git cache is stale: still the same diff
        fs::write(&a, "one\nLATER\n").unwrap();
        NAMES.lock().unwrap().clear();
        let n = diff_paths(&home, &cwd, &cwd, &dir);
        assert_eq!(
            (n.len(), diff_view(&n[0]).unwrap(), diff_stat(&n[0])),
            (2, before.clone(), Some(("2".into(), "0".into())))
        );
        assert!(!before.1.contains("LATER"));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn sealed_agent_skips_git() {
        let d = tmp("sealed");
        let (home, cwd, dir) = (d.join("home"), d.join("repo"), d.join("agent"));
        fs::create_dir_all(&dir).unwrap();
        fs::create_dir_all(&cwd).unwrap();
        let g = |a: &[&str]| {
            assert!(Command::new("git")
                .arg("-C")
                .arg(&cwd)
                .args(["-c", "user.name=t", "-c", "user.email=t@t"])
                .args(a)
                .output()
                .unwrap()
                .status
                .success())
        };
        g(&["init", "-q"]);
        fs::write(cwd.join("a.rs"), "one\n").unwrap();
        g(&["add", "."]);
        g(&["commit", "-qm", "init"]);
        fs::write(dir.join("base"), git(&cwd, &["rev-parse", "HEAD"]).unwrap()).unwrap();
        fs::write(cwd.join("a.rs"), "one\ntwo\n").unwrap();
        fs::write(dir.join("session"), "s1\n").unwrap();
        let log = home
            .join(".claude/projects")
            .join(slug(&cwd))
            .join("s1.jsonl");
        fs::create_dir_all(log.parent().unwrap()).unwrap();
        let a = cwd.join("a.rs");
        fs::write(
            &log,
            serde_json::json!({"toolUseResult": {"filePath": a}}).to_string() + "\n",
        )
        .unwrap();
        fs::write(dir.join("output.md"), "done\n").unwrap();
        let calls = || GIT_CALLS.with(|c| c.get());
        let sealed = dir.join(".diff/.sealed");
        // done just now: git runs, the diff freezes, but no seal yet (transcript may still settle)
        let before = calls();
        let n = diff_paths(&home, &cwd, &cwd, &dir);
        assert!(calls() > before);
        assert_eq!((n.len(), name(&n[0])), (1, "a.rs"));
        assert!(!sealed.exists());
        // done ≥10s ago: the next git pass seals it
        fs::File::options()
            .write(true)
            .open(dir.join("output.md"))
            .unwrap()
            .set_modified(std::time::SystemTime::now() - Duration::from_secs(20))
            .unwrap();
        NAMES.lock().unwrap().remove(&dir);
        diff_paths(&home, &cwd, &cwd, &dir);
        assert!(sealed.exists());
        // sealed: no git at all (even with no cached check), the frozen diff still shows
        NAMES.lock().unwrap().remove(&dir);
        let before = calls();
        let n = diff_paths(&home, &cwd, &cwd, &dir);
        assert_eq!(calls(), before);
        assert_eq!((n.len(), name(&n[0])), (1, "a.rs"));
        assert_eq!(diff_stat(&n[0]), Some(("1".into(), "0".into())));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn revert_touches_only_agent_files() {
        let d = tmp("revert");
        let (home, cwd, dir) = (d.join("home"), d.join("repo"), d.join("agent"));
        fs::create_dir_all(&dir).unwrap();
        fs::create_dir_all(&cwd).unwrap();
        let g = |a: &[&str]| {
            assert!(Command::new("git")
                .arg("-C")
                .arg(&cwd)
                .args(["-c", "user.name=t", "-c", "user.email=t@t"])
                .args(a)
                .output()
                .unwrap()
                .status
                .success())
        };
        g(&["init", "-q"]);
        for f in ["a.rs", "other.rs"] {
            fs::write(cwd.join(f), "base\n").unwrap();
        }
        g(&["add", "."]);
        g(&["commit", "-qm", "init"]);
        fs::write(dir.join("base"), git(&cwd, &["rev-parse", "HEAD"]).unwrap()).unwrap();
        fs::write(cwd.join("a.rs"), "agent\n").unwrap(); // committed by the agent, then edited again
        g(&["commit", "-qam", "agent"]);
        fs::write(cwd.join("a.rs"), "agent\nmore\n").unwrap();
        fs::write(cwd.join("staged.rs"), "x\n").unwrap(); // new, git added
        g(&["add", "staged.rs"]);
        fs::write(cwd.join("new.rs"), "x\n").unwrap(); // new, untracked
        fs::write(cwd.join("other.rs"), "someone else\n").unwrap(); // another agent's: not in the transcript
        fs::write(dir.join("output.md"), "result").unwrap();
        fs::write(dir.join("session"), "s1\n").unwrap();
        let log = home
            .join(".claude/projects")
            .join(slug(&cwd))
            .join("s1.jsonl");
        fs::create_dir_all(log.parent().unwrap()).unwrap();
        let edit =
            |f: &Path| serde_json::json!({"toolUseResult": {"filePath": f}}).to_string() + "\n";
        let outside = d.join("elsewhere.rs");
        fs::write(&outside, "keep\n").unwrap();
        let files: Vec<PathBuf> = ["a.rs", "staged.rs", "new.rs"]
            .iter()
            .map(|f| cwd.join(f))
            .chain([
                dir.join("output.md"),
                outside.clone(),
                cwd.join("../escape"),
            ])
            .collect();
        fs::write(&log, files.iter().map(|f| edit(f)).collect::<String>()).unwrap();
        assert_eq!(
            revert_set(&files, &cwd, &dir),
            ["a.rs", "staged.rs", "new.rs"]
        ); // no output.md, outside or `..`
        assert_eq!(revert(&home, &cwd, &cwd, &dir).unwrap(), 3);
        assert_eq!(fs::read_to_string(cwd.join("a.rs")).unwrap(), "base\n");
        assert!(!cwd.join("staged.rs").exists() && !cwd.join("new.rs").exists());
        assert_eq!(
            fs::read_to_string(cwd.join("other.rs")).unwrap(),
            "someone else\n"
        );
        assert_eq!(fs::read_to_string(dir.join("output.md")).unwrap(), "result");
        assert_eq!(fs::read_to_string(&outside).unwrap(), "keep\n");
        assert!(git(&cwd, &["ls-files"])
            .unwrap()
            .lines()
            .all(|l| l != "staged.rs"));
        assert_eq!(revert(&home, &cwd, &cwd, &dir).unwrap(), 0); // nothing left to revert
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn base_skips_commits_pulled_after_launch() {
        let d = tmp("pulled");
        let (cwd, dir) = (d.join("repo"), d.join("agent"));
        fs::create_dir_all(&dir).unwrap();
        fs::create_dir_all(&cwd).unwrap();
        let g = |date: u64, a: &[&str]| {
            assert!(Command::new("git")
                .arg("-C")
                .arg(&cwd)
                .args(["-c", "user.name=t", "-c", "user.email=t@t"])
                .args(a)
                .env("GIT_COMMITTER_DATE", format!("@{date} +0000"))
                .output()
                .unwrap()
                .status
                .success())
        };
        let a = cwd.join("a.rs");
        g(0, &["init", "-q"]);
        fs::write(&a, "one\n").unwrap();
        g(0, &["add", "."]);
        g(1_000_000_000, &["commit", "-qm", "init"]);
        fs::write(dir.join("base"), git(&cwd, &["rev-parse", "HEAD"]).unwrap()).unwrap();
        let launch = fs::metadata(dir.join("base"))
            .unwrap()
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let diff = || {
            git(
                &cwd,
                &["diff", &base(&cwd, &cwd, &dir).unwrap(), "--", "a.rs"],
            )
            .unwrap()
        };
        // the agent pulls a commit made upstream before it launched, then edits a.rs
        fs::write(&a, "one\npulled\n").unwrap();
        g(launch - 100, &["commit", "-qam", "upstream"]);
        fs::write(&a, "one\npulled\nmine\n").unwrap();
        let d1 = diff();
        assert!(d1.contains("+mine") && !d1.contains("+pulled"), "{d1}");
        // a commit the agent makes itself (after launch) still shows
        g(launch + 100, &["commit", "-qam", "agent"]);
        let d2 = diff();
        assert!(d2.contains("+mine") && !d2.contains("+pulled"), "{d2}");
        let _ = fs::remove_dir_all(&d);
    }
}
