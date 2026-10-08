//! One agent = one PTY child. A reader thread feeds a vt100 parser the UI can
//! render; status comes from a file the agent's own claude hooks write.

use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use std::collections::BTreeMap;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::{fs, thread};

pub type Res<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;
/// (thread, agent)
pub type Key = (String, String);
pub type Registry = Arc<Mutex<BTreeMap<Key, Arc<LiveAgent>>>>;

const ROWS: u16 = 40;
const COLS: u16 = 120;
const SCROLLBACK: usize = 2000;

/// The process-wide registry of agents started by this brigd. Dead agents stay
/// in it (check `alive()`) until replaced by a new start of the same key.
pub fn registry() -> Registry {
    static R: OnceLock<Registry> = OnceLock::new();
    R.get_or_init(Default::default).clone()
}

pub fn live(thread: &str, agent: &str) -> Option<Arc<LiveAgent>> {
    registry().lock().unwrap().get(&(thread.into(), agent.into())).cloned()
}

pub struct LiveAgent {
    pub thread: String,
    pub agent: String,
    /// The space the agent runs in.
    pub cwd: PathBuf,
    /// ~/.brigd/threads/<thread>/<agent>
    pub dir: PathBuf,
    pub parser: Arc<Mutex<vt100::Parser>>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    master: Mutex<Box<dyn MasterPty + Send>>,
    child: Mutex<Box<dyn Child + Send + Sync>>,
}

impl LiveAgent {
    pub fn spawn(mut cmd: CommandBuilder, thread: &str, agent: &str, cwd: &Path, dir: &Path) -> Res<LiveAgent> {
        let pair = native_pty_system().openpty(PtySize { rows: ROWS, cols: COLS, pixel_width: 0, pixel_height: 0 })?;
        cmd.cwd(cwd);
        cmd.env("TERM", "xterm-256color");
        let child = pair.slave.spawn_command(cmd)?;
        drop(pair.slave); // so the reader sees EOF when the child exits
        let mut reader = pair.master.try_clone_reader()?;
        let writer = Arc::new(Mutex::new(pair.master.take_writer()?));
        let w = writer.clone();
        let parser = Arc::new(Mutex::new(vt100::Parser::new(ROWS, COLS, SCROLLBACK)));
        let p = parser.clone();
        thread::spawn(move || {
            let mut buf = [0u8; 8192];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 {
                    break;
                }
                let mut p = p.lock().unwrap();
                p.process(&buf[..n]);
                // A child that asks where the cursor is (ESC[6n; ratatui does, at startup, so a brigd
                // run inside a brigd tab) gets an answer, or it waits seconds and fails.
                // ponytail: a query split across two reads is missed.
                if buf[..n].windows(4).any(|w| w == b"\x1b[6n") {
                    let (r, c) = p.screen().cursor_position();
                    let _ = w.lock().unwrap().write_all(format!("\x1b[{};{}R", r + 1, c + 1).as_bytes());
                }
            }
        });
        Ok(LiveAgent {
            thread: thread.into(),
            agent: agent.into(),
            cwd: cwd.into(),
            dir: dir.into(),
            parser,
            writer,
            master: Mutex::new(pair.master),
            child: Mutex::new(child),
        })
    }

    pub fn write(&self, bytes: &[u8]) -> Res<()> {
        let mut w = self.writer.lock().unwrap();
        w.write_all(bytes)?;
        Ok(w.flush()?)
    }

    pub fn resize(&self, rows: u16, cols: u16) -> Res<()> {
        self.master.lock().unwrap().resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })?;
        self.parser.lock().unwrap().screen_mut().set_size(rows, cols);
        Ok(())
    }

    pub fn alive(&self) -> bool {
        matches!(self.child.lock().unwrap().try_wait(), Ok(None))
    }

    pub fn kill(&self) {
        let _ = self.child.lock().unwrap().kill();
    }

    /// idle / working / blocked, or "starting" before the first hook fires.
    pub fn status(&self) -> String {
        read_status(&self.dir, &self.cwd)
    }
}

/// Status from the hook-written <agent>/status file. Without it (hooks did not
/// fire yet, or not at all), falls back to the tail of the session transcript.
pub fn read_status(dir: &Path, cwd: &Path) -> String {
    if let Ok(s) = fs::read_to_string(dir.join("status")) {
        if !s.trim().is_empty() {
            return s.trim().into();
        }
    }
    let Ok(session) = fs::read_to_string(dir.join("session")) else { return "starting".into() };
    let home = PathBuf::from(std::env::var("HOME").unwrap_or_default());
    let path = home.join(".claude/projects").join(crate::tree::slug(cwd)).join(format!("{}.jsonl", session.trim()));
    transcript_status(&path).unwrap_or_else(|| "starting".into())
}

/// Last user/assistant record: assistant end_turn => idle, anything else => working.
fn transcript_status(path: &Path) -> Option<String> {
    let mut f = fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    f.seek(SeekFrom::Start(len.saturating_sub(256 * 1024))).ok()?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf);
    for line in text.lines().rev() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { continue };
        match v["type"].as_str() {
            Some("assistant") if v["message"]["stop_reason"] == "end_turn" => return Some("idle".into()),
            Some("assistant" | "user") => return Some("working".into()),
            _ => {}
        }
    }
    None
}

/// Is `pid` a running process? (`kill -0`)
pub fn pid_alive(pid: &str) -> bool {
    std::process::Command::new("kill").args(["-0", pid.trim()]).stderr(std::process::Stdio::null()).status().is_ok_and(|s| s.success())
}

/// <agent dir>/terminated: written when Mark terminates the agent, cleared when it is started again.
pub const TERMINATED: &str = "terminated";

/// A random v4 UUID from /dev/urandom.
pub fn new_uuid() -> Res<String> {
    let mut b = [0u8; 16];
    fs::File::open("/dev/urandom")?.read_exact(&mut b)?;
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    Ok(format!("{}-{}-{}-{}-{}", &h[..8], &h[8..12], &h[12..16], &h[16..20], &h[20..]))
}

fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Inline --settings JSON whose hooks write idle/working/blocked to <dir>/status.
pub fn hooks_json(dir: &Path) -> String {
    let file = sh_quote(&dir.join("status").to_string_lossy());
    let set = |s: &str| serde_json::json!([{"hooks": [{"type": "command", "command": format!("printf {s} > {file}")}]}]);
    let mut blocked = set("blocked");
    // idle_prompt notifications fire after a quiet minute; only permission prompts mean blocked.
    blocked[0]["matcher"] = "permission_prompt".into();
    serde_json::json!({"hooks": {
        "UserPromptSubmit": set("working"),
        "PreToolUse": set("working"),
        "PostToolUse": set("working"),
        "Notification": blocked,
        "Stop": set("idle"),
    }})
    .to_string()
}

pub fn register(a: LiveAgent) -> Arc<LiveAgent> {
    let a = Arc::new(a);
    registry().lock().unwrap().insert((a.thread.clone(), a.agent.clone()), a.clone());
    a
}

/// `--session-id <new> --settings <hooks>` for a fresh session in <dir>: saves the id to
/// <dir>/session and clears a stale <dir>/status.
pub fn session_args(dir: &Path) -> Res<[String; 4]> {
    let _ = fs::remove_file(dir.join("status"));
    let _ = fs::remove_file(dir.join(TERMINATED));
    let session = new_uuid()?;
    fs::write(dir.join("session"), &session)?;
    Ok(["--session-id".into(), session, "--settings".into(), hooks_json(dir)])
}

/// Starts `claude <flags> --session-id <new> --settings <hooks> "<prompt>"` in `cwd`,
/// saves the session id to <dir>/session and registers the agent.
pub fn start_claude(thread: &str, agent: &str, cwd: &Path, dir: &Path, flags: &[String], prompt: &str) -> Res<Arc<LiveAgent>> {
    fs::create_dir_all(dir)?;
    let mut cmd = CommandBuilder::new("claude");
    cmd.args(flags);
    cmd.args(session_args(dir)?);
    cmd.arg(prompt); // last, and flags use --flag=value so variadic options cannot swallow it
    Ok(register(LiveAgent::spawn(cmd, thread, agent, cwd, dir)?))
}

/// Reopens an agent's saved session (`claude --resume <session>`) in its space,
/// unless it is still live. Returns the live agent. A stray (`stray agents/<name>`,
/// a claude typed in a brigd terminal) has no flow entry: it reopens in its saved cwd.
pub fn resume_agent(thread: &str, agent: &str) -> Res<Arc<LiveAgent>> {
    if let Some(a) = live(thread, agent).filter(|a| a.alive()) {
        return Ok(a);
    }
    let dir = crate::thread_dir(thread).join(agent);
    let mut cmd = CommandBuilder::new("claude");
    let cwd = if Path::new(agent).starts_with(crate::tree::STRAY) {
        cmd.args(["--permission-mode", "auto"]);
        PathBuf::from(fs::read_to_string(dir.join("cwd")).map_err(|e| format!("{thread}/{agent}: no cwd: {e}"))?.trim())
    } else {
        let (flow, state) = crate::load(thread)?;
        let a = flow.stages.iter().flatten().find(|a| a.name == agent).ok_or_else(|| format!("no agent {agent} in thread {thread}"))?;
        cmd.args(crate::claude_flags(thread, a));
        crate::tree::space_cwd(&crate::root(), Path::new(&state.repo), thread, a.worktree.as_deref().unwrap_or(""))
    };
    let session = fs::read_to_string(dir.join("session")).map_err(|e| format!("{thread}/{agent}: no session: {e}"))?;
    let _ = fs::remove_file(dir.join("status"));
    let _ = fs::remove_file(dir.join(TERMINATED));
    // ponytail: assumes --resume keeps the session id (subagent scan reads <session>/subagents); re-read it if not.
    cmd.args(["--resume", session.trim(), "--settings", &hooks_json(&dir)]);
    Ok(register(LiveAgent::spawn(cmd, thread, agent, &cwd, &dir)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pty_prints_hi() {
        let mut cmd = CommandBuilder::new("sh");
        cmd.args(["-c", "printf hi"]);
        let tmp = std::env::temp_dir();
        let a = LiveAgent::spawn(cmd, "t", "a", &tmp, &tmp).unwrap();
        for _ in 0..50 {
            if a.parser.lock().unwrap().screen().contents().contains("hi") {
                return;
            }
            thread::sleep(std::time::Duration::from_millis(100));
        }
        panic!("screen: {:?}", a.parser.lock().unwrap().screen().contents());
    }

    #[test]
    fn cursor_query_is_answered() {
        let mut cmd = CommandBuilder::new("sh");
        cmd.args(["-c", r"stty raw -echo; printf '\033[6n'; dd bs=1 count=6 2>/dev/null | od -c | head -1"]);
        let tmp = std::env::temp_dir();
        let a = LiveAgent::spawn(cmd, "t", "a", &tmp, &tmp).unwrap();
        for _ in 0..50 {
            if a.parser.lock().unwrap().screen().contents().contains("033   [   1   ;   1   R") {
                return;
            }
            thread::sleep(std::time::Duration::from_millis(100));
        }
        panic!("screen: {:?}", a.parser.lock().unwrap().screen().contents());
    }

    #[test]
    fn uuid_and_hooks() {
        let u = new_uuid().unwrap();
        assert_eq!(u.len(), 36);
        assert_eq!(&u[14..15], "4");
        let v: serde_json::Value = serde_json::from_str(&hooks_json(Path::new("/x/it's"))).unwrap();
        assert_eq!(v["hooks"]["Stop"][0]["hooks"][0]["command"], r"printf idle > '/x/it'\''s/status'");
        assert_eq!(v["hooks"]["Notification"][0]["matcher"], "permission_prompt");
    }
}
