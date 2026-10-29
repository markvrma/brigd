//! The '+' new-thread window: its form state, validation, submit, and the background planner/execute threads it starts.

use super::App;
use ratatui::text::Span;
use std::path::PathBuf;
use std::sync::mpsc;
use std::{env, fs};

/// Width of the new-thread window.
pub(super) const NEWT_W: u16 = 60;

/// What the planner and execute threads of a '+' new thread report to the run loop.
pub(super) enum Bg {
    /// (thread, repo): planned, waiting for the y/n confirm.
    Planned(String, String),
    /// (thread, message): planning failed.
    Failed(String, String),
    Finished(String, Result<(), String>),
}

/// The '+' new-thread window: dir, name and task fields; once submitted, read-only while planning.
#[derive(Debug, Default, PartialEq)]
pub(super) struct Newt {
    /// dir, name, task.
    pub(super) fields: [String; 3],
    /// Index into `fields` of the focused one.
    pub(super) focus: usize,
    pub(super) err: Option<String>,
    /// Enter on the task field passed validation: asking "Use plan-review-go?".
    pub(super) ask: bool,
    /// Submitted: the planner thread runs and the fields are read-only.
    pub(super) planning: bool,
    /// Esc while planning: the box is gone but planning goes on.
    pub(super) hidden: bool,
}

/// `s` cut into rows of at most `w` display columns (and at each '\n'); always at least one row.
pub(super) fn chunk(s: &str, w: usize) -> Vec<String> {
    let w = w.max(1);
    let (mut rows, mut row, mut used) = (vec![], String::new(), 0);
    for c in s.chars() {
        let cw = if c == '\n' {
            0
        } else {
            Span::raw(c.to_string()).width()
        };
        if c == '\n' || (used + cw > w && used > 0) {
            rows.push(std::mem::take(&mut row));
            used = 0;
        }
        if c != '\n' {
            row.push(c);
            used += cw;
        }
    }
    rows.push(row);
    rows
}

impl App {
    pub(super) fn open_newt(&mut self, repo: PathBuf) {
        // A hidden box still planning is tracked in `newt`: replacing it would lose its inputs on failure.
        if let Some(n) = self.newt.as_mut().filter(|n| n.planning) {
            n.hidden = false;
        } else {
            self.newt = Some(Newt {
                fields: [repo.display().to_string(), String::new(), String::new()],
                focus: 1,
                ..Default::default()
            });
        }
    }

    /// The window with the dir field focused, for a thread in any directory.
    pub(super) fn open_newt_anywhere(&mut self, repo: PathBuf) {
        self.open_newt(repo);
        if let Some(n) = self.newt.as_mut().filter(|n| !n.planning) {
            n.focus = 0;
        }
    }

    /// Where a new thread goes by default: the highlighted row's space, else the shown one, else the cwd.
    pub(super) fn new_dir(&self) -> PathBuf {
        let space = self.rows.get(self.sel).map_or(&self.space, |r| &r.space);
        if space.as_os_str().is_empty() {
            env::current_dir().unwrap_or_default()
        } else {
            space.clone()
        }
    }

    pub(super) fn tx(&mut self) -> mpsc::Sender<Bg> {
        self.bg.get_or_insert_with(mpsc::channel).0.clone()
    }

    /// The dir field as a path: `~` expands to $HOME.
    fn newt_path(typed: &str) -> PathBuf {
        match (typed, typed.strip_prefix("~/"), env::var_os("HOME")) {
            ("~", _, Some(h)) => PathBuf::from(h),
            (_, Some(rest), Some(h)) => PathBuf::from(h).join(rest),
            _ => PathBuf::from(typed),
        }
    }

    /// The first failing check of the new-thread fields (focus index, message); reserves nothing.
    pub(super) fn newt_check(fields: &[String; 3]) -> Option<(usize, String)> {
        let typed = fields[0].trim();
        match fs::canonicalize(Self::newt_path(typed))
            .ok()
            .filter(|p| p.is_dir())
        {
            None => Some((0, format!("no such directory: {typed}"))),
            Some(_) => crate::store::check_new_thread(fields[1].trim())
                .err()
                .map(|e| (1, e.to_string()))
                .or_else(|| {
                    fields[2]
                        .trim()
                        .is_empty()
                        .then(|| (2, "task is empty".into()))
                }),
        }
    }

    /// Enter in the new-thread window: resolves the dir, reserves the name, then plans off the UI thread.
    /// `prg`: the user answered yes to "Use plan-review-go?".
    pub(super) fn submit_newt(&mut self, fields: [String; 3], prg: bool) {
        let path = Self::newt_path(fields[0].trim());
        let (name, mut task) = (fields[1].trim().to_string(), fields[2].trim().to_string());
        if prg {
            task = crate::with_prg(&task);
        }
        let dir = crate::store::thread_dir(&name);
        // Only the first failing check shows; create_dir reserves the name and tree::build hides
        // the dir until flowmap.json exists.
        let failed = Self::newt_check(&fields).or_else(|| {
            fs::create_dir_all(crate::store::root().join("threads"))
                .and_then(|_| fs::create_dir(&dir))
                .err()
                .map(|e| (1, e.to_string()))
        });
        if let Some((focus, err)) = failed {
            self.newt = Some(Newt {
                fields,
                focus,
                err: Some(err),
                ..Default::default()
            });
            return;
        }
        let repo = fs::canonicalize(&path).unwrap_or(path);
        self.newt = Some(Newt {
            fields: [repo.display().to_string(), name.clone(), task.clone()],
            focus: 2,
            planning: true,
            ..Default::default()
        });
        self.planning.insert(name.clone());
        self.flash(format!("planning {name}…"));
        let tx = self.tx();
        // ponytail: no cancel; quitting brigd mid-plan orphans the `claude -p` planner.
        std::thread::spawn(move || {
            let r = std::panic::catch_unwind(|| {
                crate::flow::plan(&task, &repo).and_then(|flow| {
                    crate::store::create_thread(
                        &crate::store::root(),
                        &name,
                        &repo.to_string_lossy(),
                        &flow,
                    )?;
                    Ok(fs::write(dir.join("FLOWPLAN"), super::flow_text(&flow))?)
                })
            })
            .unwrap_or_else(|_| Err("planner panicked".into()));
            let _ = tx.send(match r {
                Ok(()) => Bg::Planned(name, repo.to_string_lossy().into_owned()),
                Err(e) => {
                    let _ = fs::remove_dir_all(&dir);
                    Bg::Failed(name, e.to_string())
                }
            });
        });
    }

    pub(super) fn on_bg(&mut self, m: Bg) {
        match m {
            Bg::Failed(name, e) => {
                self.planning.remove(&name);
                match &mut self.newt {
                    Some(n) if n.planning && n.fields[1] == name => {
                        (n.planning, n.hidden, n.err) = (false, false, Some(e))
                    }
                    _ => self.flash(format!("plan {name}: {e}")),
                }
            }
            Bg::Planned(name, repo) => {
                self.planning.remove(&name);
                if self
                    .newt
                    .as_ref()
                    .is_some_and(|n| n.planning && n.fields[1] == name)
                {
                    self.newt = None;
                }
                self.refresh();
                self.open_flowplan(&name);
                if self.confirm_run.is_some() {
                    self.queued.push((name, repo));
                } else {
                    self.confirm_run = Some((name, repo));
                }
            }
            Bg::Finished(name, Ok(())) => self.flash(format!("thread {name} done")),
            Bg::Finished(name, Err(e)) => self.flash(format!("thread {name}: {e}")),
        }
    }
}
