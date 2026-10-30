//! The TUI's tests: rendering, key and mouse handling, and the helpers, driven through `App`.

use super::flowplan::{flow_lines, SPIN};
use super::input::key_bytes;
use super::markdown::{diff_lines, md_text};
use super::newt::chunk;
use super::sidebar::{brick_banner, flatten, hit, layout, max_offset};
use super::tabs::{pane_rects, Pane};
use super::term::base64;
use super::theme::{theme_index, THEMES};
use super::threads::{fresh_name, Act};
use super::*;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::style::Modifier;

fn key(code: KeyCode, m: KeyModifiers) -> KeyEvent {
    KeyEvent::new(code, m)
}

fn one_pane(list: Vec<Tab>) -> ThreadPanes {
    ThreadPanes {
        list: vec![Pane {
            list,
            ..Default::default()
        }],
        focus: 0,
    }
}

#[test]
fn live_status_beats_saved() {
    use Status::*;
    assert_eq!(agent_glyph_status(Some(Working), Done), Running);
    assert_eq!(agent_glyph_status(Some(Idle), Done), Idle);
    assert_eq!(agent_glyph_status(Some(Blocked), Failed), Blocked);
    assert_eq!(agent_glyph_status(None, Done), Done);
    assert_eq!(agent_glyph_status(None, Pending), Off);
}

#[test]
fn keys_to_bytes() {
    let none = KeyModifiers::NONE;
    assert_eq!(
        key_bytes(key(KeyCode::Char('c'), KeyModifiers::CONTROL), false),
        [3]
    );
    assert_eq!(
        key_bytes(key(KeyCode::Char('A'), KeyModifiers::SHIFT), false),
        b"A"
    );
    assert_eq!(
        key_bytes(key(KeyCode::Char('é'), none), false),
        "é".as_bytes()
    );
    assert_eq!(key_bytes(key(KeyCode::Enter, none), false), b"\r");
    assert_eq!(
        key_bytes(key(KeyCode::Enter, KeyModifiers::ALT), false),
        b"\x1b\r"
    );
    assert_eq!(key_bytes(key(KeyCode::Backspace, none), false), [0x7f]);
    assert_eq!(key_bytes(key(KeyCode::Up, none), false), b"\x1b[A");
    assert_eq!(key_bytes(key(KeyCode::Up, none), true), b"\x1bOA");
    assert_eq!(
        key_bytes(key(KeyCode::BackTab, KeyModifiers::SHIFT), false),
        b"\x1b[Z"
    );
    assert_eq!(
        key_bytes(key(KeyCode::Esc, KeyModifiers::ALT), false),
        [0x1b]
    );
    assert_eq!(key_bytes(key(KeyCode::F(1), none), false), b"\x1bOP");
    assert_eq!(key_bytes(key(KeyCode::F(12), none), false), b"\x1b[24~");
    assert_eq!(key_bytes(key(KeyCode::CapsLock, none), false), b"");
}

#[test]
fn markdown_lines() {
    let t = md_text(
        "# Title\nsome **bold** x\n```\n**raw**\n---\n```\n\n---",
        30,
    );
    let s: Vec<String> = t
        .lines
        .iter()
        .map(|l| l.spans.iter().map(|s| &*s.content).collect())
        .collect();
    let h = &t.lines[s.iter().position(|l| l == "Title").unwrap()];
    assert_eq!(h.style.fg, Some(pal().mauve)); // heading style sits on the line, not the spans
    assert!(h.style.add_modifier.contains(Modifier::BOLD));
    assert!(!s.iter().any(|l| l.contains("```")));
    assert!(s.iter().any(|l| l == "**raw**")); // no inline parsing in code
    assert!(s.iter().any(|l| l == "---")); // a rule inside code stays literal
    assert!(s.iter().any(|l| l == &"─".repeat(30)));
}

#[test]
fn markdown_table() {
    let t = md_text("| a | b |\n|---|---|\n| a long cell that wraps | x |", 20);
    let s: Vec<String> = t
        .lines
        .iter()
        .map(|l| l.spans.iter().map(|s| &*s.content).collect())
        .collect();
    assert!(t.lines.iter().all(|l| l.width() <= 20), "{s:#?}");
    assert!(s[0].starts_with('┌'));
    assert!(s.iter().any(|l| l.contains("│ a")));
}

#[test]
fn theme_names_resolve() {
    for (i, t) in THEMES.iter().enumerate() {
        assert_eq!(theme_index(t.name), i);
    }
    assert_eq!(THEMES[theme_index("nope")].name, "catppuccin-mocha");
}

#[test]
fn diff_colors() {
    let l = diff_lines("@@ -1,2 +1,2 @@\n a\n-b\n+c");
    let fg: Vec<_> = l.iter().map(|l| l.style.fg).collect();
    assert_eq!(
        fg,
        [Some(pal().sky), None, Some(pal().red), Some(pal().green)]
    );
}

#[test]
fn click_hits_tree_rows() {
    let node = |kind, label: &str, children| Node {
        kind,
        label: label.into(),
        path: PathBuf::from(format!("/{label}")),
        children,
        stage: None,
        worktree: None,
    };
    let agent = node(
        Kind::Agent,
        "a",
        vec![node(Kind::Output, "output.md", vec![])],
    );
    let tree = node(
        Kind::Space,
        "repo",
        vec![node(
            Kind::Thread,
            "t",
            vec![agent, node(Kind::Agent, "b", vec![])],
        )],
    );
    let mut rows = Vec::new();
    flatten(&tree, 0, &tree.path, "", &HashSet::new(), &mut rows);
    // Spaces start open, threads folded.
    assert_eq!(rows.len(), 2);
    let open_t = HashSet::from([(tree.path.clone(), PathBuf::from("/t"))]);
    let mut rows = Vec::new();
    flatten(&tree, 0, &tree.path, "", &open_t, &mut rows);
    // Thread unfolded: agents folded.
    let labels: Vec<_> = rows
        .iter()
        .map(|r| (r.depth, r.label.as_str(), r.thread.as_str()))
        .collect();
    assert_eq!(
        labels,
        [(0, "repo", ""), (1, "t", "t"), (2, "a", "t"), (2, "b", "t")]
    );
    let inner = Rect::new(1, 1, 34, 3); // lines 1..=3: repo, t, a
    assert_eq!(hit(inner, 0, &rows, 5, 1), Some((0, false)));
    assert_eq!(hit(inner, 1, &rows, 10, 2), Some((2, false))); // scrolled by one
    assert_eq!(hit(inner, 0, &rows, 1 + 6, 3), Some((2, true))); // depth 2 arrow at x 6..8
    assert_eq!(hit(inner, 0, &rows, 1 + 8, 3), Some((2, false)));
    assert_eq!(hit(inner, 1, &rows, 1 + 6, 3), Some((3, false))); // "b" has no children: no arrow
    assert_eq!(hit(inner, 2, &rows, 5, 3), None); // past the last row
    assert_eq!(hit(inner, 0, &rows, 5, 4), None); // below the list
    assert_eq!(hit(inner, 0, &rows, 0, 1), None); // on the border
    assert_eq!(max_offset(&rows, 3), 1);
    // A blank line above a later thread and a later space, none under a space.
    let two = node(
        Kind::Space,
        "r",
        vec![
            node(Kind::Thread, "t", vec![]),
            node(Kind::Thread, "u", vec![]),
        ],
    );
    let mut rows = Vec::new();
    flatten(&two, 0, &two.path, "", &HashSet::new(), &mut rows);
    flatten(&tree, 0, &tree.path, "", &open_t, &mut rows);
    let ys: Vec<_> = layout(&rows, 0, 20).into_iter().map(|(y, _)| y).collect();
    assert_eq!(ys, [0, 1, 3, 5, 6, 7, 8]); // r t _ u _ repo t a b
                                           // Unfolding agent "a" shows its output.md.
    let mut toggled = open_t.clone();
    toggled.insert((tree.path.clone(), PathBuf::from("/a")));
    let mut rows = Vec::new();
    flatten(&tree, 0, &tree.path, "", &toggled, &mut rows);
    assert_eq!((rows[3].kind.clone(), rows[3].depth), (Kind::Output, 3));
}

#[test]
fn alt_hl_cycles_tabs() {
    let view = || Tab::View {
        path: PathBuf::new(),
        title: String::new(),
        text: String::new(),
        scroll: 0,
        rel: None,
    };
    let mut app = App::default();
    app.tabs
        .insert(String::new(), one_pane(vec![view(), view()]));
    app.on_key(key(KeyCode::Char('l'), KeyModifiers::ALT)); // from the sidebar
    assert_eq!((app.tabs[""].list[0].active, app.focus_main), (1, true));
    app.on_key(key(KeyCode::Char('˙'), KeyModifiers::NONE));
    assert_eq!(app.tabs[""].list[0].active, 0);
}

#[test]
fn tab_bar_follows_active_tab() {
    let view = |n: usize| Tab::View {
        path: PathBuf::new(),
        title: format!("tab-number-{n}"),
        text: String::new(),
        scroll: 0,
        rel: None,
    };
    let mut app = App::default();
    app.tabs
        .insert(String::new(), one_pane((0..10).map(view).collect()));
    let mut term = Terminal::new(ratatui::backend::TestBackend::new(100, 10)).unwrap();
    term.draw(|f| app.draw(f)).unwrap();
    assert_eq!(app.tabs[""].list[0].spans[0].2, 0);
    for _ in 0..9 {
        app.on_key(key(KeyCode::Char('l'), KeyModifiers::ALT));
    }
    term.draw(|f| app.draw(f)).unwrap();
    let t = &app.tabs[""].list[0];
    let (first, (a, b, last)) = (t.spans[0].2, *t.spans.last().unwrap());
    assert!(first > 0 && last == 9); // the last tab is shown, fully
    assert!(b <= t.bar.right() && b - a == "tab-number-9".len() as u16 + 2 + 5); // " title " + fade + "× "
                                                                                 // A click on a tab maps through the scrolled window.
    let (a, _, i) = t.spans[0];
    app.on_mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: a,
        row: 0,
        modifiers: KeyModifiers::NONE,
    });
    assert_eq!(app.tabs[""].list[0].active, i);
}

#[test]
fn tab_x_closes_that_tab() {
    let view = |n: usize| Tab::View {
        path: PathBuf::new(),
        title: format!("t{n}"),
        text: String::new(),
        scroll: 0,
        rel: None,
    };
    let mut app = App::default();
    app.tabs
        .insert(String::new(), one_pane((0..3).map(view).collect()));
    app.focused_pane().active = 1;
    let mut term = Terminal::new(ratatui::backend::TestBackend::new(100, 10)).unwrap();
    term.draw(|f| app.draw(f)).unwrap();
    let (_, x1, _) = app.tabs[""].list[0].spans[2];
    app.on_mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: x1 - 2, // the ×
        row: 0,
        modifiers: KeyModifiers::NONE,
    });
    let t = &app.tabs[""].list[0];
    assert_eq!((t.list.len(), t.active), (2, 1));
}

#[test]
fn header_plus_is_a_button() {
    let mut app = App::default();
    let mut term = Terminal::new(ratatui::backend::TestBackend::new(100, 10)).unwrap();
    term.draw(|f| app.draw(f)).unwrap();
    let r = app.header_plus;
    assert!(r.width == 2 && r.y == 0 && app.plus_btns.iter().any(|(b, _)| *b == r));
    app.on_mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: r.x,
        row: 0,
        modifiers: KeyModifiers::NONE,
    });
    assert_eq!(app.newt.as_ref().unwrap().focus, 0);
}

#[test]
fn flow_md_draws_step_tree() {
    let s = |t: &str| {
        flow_lines(t).map(|ls| {
            ls.iter()
                .map(|l| l.spans.iter().map(|s| &*s.content).collect::<String>())
                .collect::<Vec<_>>()
        })
    };
    // a decision whose branches rejoin: no ← tags needed
    let l = s("```mermaid\nflowchart TD\n  A[Read] --> B{ok?}\n  B -->|yes| C[Ship] \n  B -->|no| D[Fix]\n  C & D --> E[Done]\n```").unwrap();
    assert_eq!(
        l,
        [
            "✔  Read",
            "│",
            "✔  ok?",
            "│",
            "├─ ✔  Ship  · yes",
            "└─ ✔  Fix  · no",
            "│",
            "✔  Done"
        ]
    );
    // uneven branches: each node names its parents when they aren't the whole step above
    let l = s(
        "flowchart TD\n A[Start] --> B[Long 1] --> D[Long 2] --> E[Merge]\n A --> C[Short] --> E",
    )
    .unwrap();
    assert_eq!(
        l,
        [
            "✔  Start",
            "│",
            "├─ ✔  Long 1",
            "└─ ✔  Short",
            "│",
            "✔  Long 2  ← Long 1",
            "│",
            "✔  Merge  ← Long 2, Short"
        ]
    );
    assert_eq!(
        s("flowchart LR\n A --> B --> A").unwrap(),
        ["✔  A  ← B", "│", "✔  B"]
    ); // a loop terminates
    for bad in [
        "",
        "garbage ((( -->",
        "# notes\nno diagram",
        "flowchart TD\n A -.-> B",
    ] {
        assert!(s(bad).is_none(), "{bad}"); // → raw markdown
    }
}

#[test]
fn flowplan_view_draws_step_tree() {
    let agent = |name: &str, wt: Option<&str>| crate::store::Agent {
        name: name.into(),
        agent: "claude".into(),
        model: "haiku".into(),
        effort: "low".into(),
        worktree: wt.map(String::from),
        task: format!(
            "task of {name}\nmore\n\n{}tail-of-{name}",
            "word ".repeat(40)
        ),
        files: vec![],
        mode: "default".into(),
    };
    let flow = crate::store::Flow {
        goal: "ship it".into(),
        stages: vec![
            vec![agent("check-db", None), agent("trace-auth", Some("login"))],
            vec![agent("review", None)],
        ],
    };
    let text = flow_text(&flow);
    assert!(text.contains("tail-of-trace-auth"), "{text}");
    assert!(
        text.contains("│  ├─ ◇ check-db  ") && text.ends_with("│\n└  log\n"),
        "{text}"
    );

    let path = PathBuf::from("/th/FLOWPLAN");
    let mut state = crate::store::State {
        thread: "th".into(),
        ..Default::default()
    };
    state.agents.insert(
        "check-db".into(),
        crate::store::AgentState {
            status: Status::Done,
        },
    );
    let mut app = App::default();
    app.agent_status
        .insert(("th".into(), "trace-auth".into()), Status::Running);
    app.plans.insert(path.clone(), (flow, state));
    let tab = Tab::View {
        path,
        title: String::new(),
        text: text + "   start check-db in /repo\n",
        scroll: 0,
        rel: None,
    };
    app.tabs.insert(String::new(), one_pane(vec![tab]));
    let mut term = Terminal::new(ratatui::backend::TestBackend::new(120, 60)).unwrap();
    term.draw(|f| app.draw(f)).unwrap();
    let b = term.backend().buffer();
    let rows: Vec<String> = (0..b.area.height)
        .map(|y| {
            (0..b.area.width)
                .map(|x| b.cell((x, y)).map_or(" ", |c| c.symbol()))
                .collect()
        })
        .collect();
    let has = |s: &str| rows.iter().any(|r| r.contains(s));
    assert!(
        has("◆  ship it") && has("│  ├─ ✔ check-db") && has("│  │    task of check-db"),
        "{}",
        rows.join("\n")
    );
    assert!(SPIN.iter().any(|f| has(&format!("│  └─ {f} trace-auth"))) && has("wt login"));
    assert!(
        SPIN.iter()
            .any(|f| has(&format!("{f}  stage 1 · 2 parallel")))
            && has("◇  stage 2 · 1 agent")
    );
    assert!(has("└  log") && has("start check-db in /repo"));
    // Long tasks collapse to their first 10 words and a "...".
    assert!(
        has("│  │    task of check-db more word word word word word word ...")
            && !has("tail-of-check-db"),
        "{}",
        rows.join("\n")
    );
    // Clicking check-db's "..." expands it: wrapped whole, not truncated. Again collapses.
    let click = |app: &mut App| {
        let r = app
            .task_btns
            .iter()
            .find(|(_, (_, a))| a == "check-db")
            .unwrap()
            .0;
        let dots: String = rows[r.y as usize]
            .chars()
            .skip(r.x as usize)
            .take(3)
            .collect();
        assert_eq!(dots, "...");
        app.on_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: r.x,
            row: r.y,
            modifiers: KeyModifiers::NONE,
        });
    };
    click(&mut app);
    term.draw(|f| app.draw(f)).unwrap();
    let shown = |term: &Terminal<ratatui::backend::TestBackend>, s: &str| {
        let b = term.backend().buffer();
        (0..b.area.height).any(|y| {
            (0..b.area.width)
                .map(|x| b.cell((x, y)).map_or(" ", |c| c.symbol()))
                .collect::<String>()
                .contains(s)
        })
    };
    assert!(
        shown(&term, "│  │    more")
            && shown(&term, "tail-of-check-db")
            && !shown(&term, "tail-of-review")
    );
    let r = app
        .task_btns
        .iter()
        .find(|(_, (_, a))| a == "check-db")
        .unwrap()
        .0;
    app.on_mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: r.x + 3,
        row: r.y,
        modifiers: KeyModifiers::NONE,
    });
    term.draw(|f| app.draw(f)).unwrap();
    assert!(!shown(&term, "tail-of-check-db"));
}

#[test]
fn tab_cap_evicts_oldest() {
    let mut app = App::default();
    let paths = |t: &App, th: &str| {
        t.tabs[th].list[0]
            .list
            .iter()
            .map(|t| {
                if let Tab::View { path, .. } = t {
                    path.clone()
                } else {
                    PathBuf::new()
                }
            })
            .collect::<Vec<_>>()
    };
    for i in 1..=6 {
        app.open_view("t".into(), PathBuf::from(format!("/nope/{i}")));
    }
    let want: Vec<_> = (2..=6)
        .map(|i| PathBuf::from(format!("/nope/{i}")))
        .collect();
    assert_eq!(
        (paths(&app, "t"), app.tabs["t"].list[0].active),
        (want.clone(), 4)
    );
    app.open_view("t".into(), PathBuf::from("/nope/2")); // already open: only activated
    assert_eq!(
        (paths(&app, "t"), app.tabs["t"].list[0].active),
        (want.clone(), 0)
    );
    app.open_view("u".into(), PathBuf::from("/nope/7"));
    assert_eq!(
        (paths(&app, "t"), app.tabs["u"].list[0].list.len()),
        (want, 1)
    );
}

#[test]
fn split_gives_new_pane_own_tab_bar() {
    let main = Rect::new(41, 0, 79, 19);
    let widths: Vec<_> = pane_rects(main, 2)
        .iter()
        .map(|(bar, body)| (bar.x, bar.width, body.y, body.height))
        .collect();
    assert_eq!(widths, [(41, 39, 1, 18), (81, 39, 1, 18)]);
    let mut app = App {
        thread: "t".into(),
        ..Default::default()
    };
    app.open_view("t".into(), PathBuf::from("/nope/1"));
    app.open_view("t".into(), PathBuf::from("/nope/2"));
    let p = app.tabs.get_mut("t").unwrap();
    p.list.push(Pane::default()); // what split does, then new_term; a viewer stands in for the shell
    p.focus = 1;
    app.open_view("t".into(), PathBuf::from("/nope/3"));
    let mut term = Terminal::new(ratatui::backend::TestBackend::new(120, 20)).unwrap();
    term.draw(|f| app.draw(f)).unwrap();
    let p = &app.tabs["t"];
    assert_eq!(
        p.list
            .iter()
            .map(|t| (t.list.len(), t.bar, t.spans.len()))
            .collect::<Vec<_>>(),
        [
            (2, pane_rects(main, 2)[0].0, 2),
            (1, pane_rects(main, 2)[1].0, 1)
        ]
    );
    assert_eq!(term.backend().buffer().cell((80, 5)).unwrap().symbol(), "│");
    app.split();
    assert_eq!(
        (
            app.tabs["t"].list.len(),
            app.msg.as_ref().unwrap().0.as_str()
        ),
        (2, "max 2 panes")
    );
    app.open_view("t".into(), PathBuf::from("/nope/1")); // open in pane 0: focused there, not opened twice
    assert_eq!(
        (app.tabs["t"].focus, app.tabs["t"].list[1].list.len()),
        (0, 1)
    );
    app.on_key(key(KeyCode::Char('o'), KeyModifiers::CONTROL));
    app.on_key(key(KeyCode::Char('o'), KeyModifiers::NONE));
    app.thread = "u".into(); // another thread and back: both panes and the focus stay
    term.draw(|f| app.draw(f)).unwrap();
    app.thread = "t".into();
    assert_eq!((app.tabs["t"].list.len(), app.tabs["t"].focus), (2, 1));
    app.on_key(key(KeyCode::Char('o'), KeyModifiers::CONTROL));
    app.on_key(key(KeyCode::Char('w'), KeyModifiers::NONE)); // pane 1's last tab: the pane goes
    let p = &app.tabs["t"];
    assert_eq!((p.list.len(), p.focus, p.list[0].list.len()), (1, 0, 2));
}

#[test]
fn terminal_folder_under_thread() {
    let node = |kind, label: &str, path: &str, children| Node {
        kind,
        label: label.into(),
        path: PathBuf::from(path),
        children,
        stage: None,
        worktree: None,
    };
    let t = node(
        Kind::Thread,
        "t",
        "/t",
        vec![
            node(Kind::Output, "FLOWPLAN", "/t/FLOWPLAN", vec![]),
            node(Kind::Agent, "a", "/t/a", vec![]),
        ],
    );
    let mut tree = vec![node(
        Kind::Space,
        "repo",
        "/repo",
        vec![
            t,
            node(
                Kind::Thread,
                "u",
                "/u",
                vec![node(Kind::Agent, "b", "/u/b", vec![])],
            ),
        ],
    )];
    let terms = [
        ("t", "term-2"),
        ("t", "term-1"),
        ("zz", "term-3"),
        ("u", "term-4"),
    ]
    .map(|(a, b)| (a.to_string(), b.to_string()));
    add_terminals(&mut tree, &terms);
    let mut rows = Vec::new();
    let open = HashSet::from(["/t", "/u"].map(|p| (tree[0].path.clone(), PathBuf::from(p))));
    flatten(&tree[0], 0, &tree[0].path, "", &open, &mut rows);
    let labels: Vec<_> = rows
        .iter()
        .map(|r| (r.label.as_str(), r.thread.as_str()))
        .collect();
    assert_eq!(
        labels,
        [
            ("repo", ""),
            ("t", "t"),
            ("FLOWPLAN", "t"),
            ("terminal", "t"),
            ("term-1", "t"),
            ("term-2", "t"),
            ("a", "t"),
            ("u", "u"),
            ("terminal", "u"),
            ("term-4", "u"),
            ("b", "u")
        ]
    );
    assert_eq!(
        (rows[4].kind.clone(), rows[4].path.clone()),
        (Kind::Terminal, PathBuf::from("/t/.terminal/term-1"))
    );
}

#[test]
fn fresh_names() {
    assert_eq!(fresh_name([].into_iter()), "term-1");
    assert_eq!(fresh_name(["term-1", "a", "term-3"].into_iter()), "term-2");
}

#[test]
fn right_click_opens_terminate_menu() {
    let node = |kind, label: &str, children| Node {
        kind,
        label: label.into(),
        path: PathBuf::from(format!("/{label}")),
        children,
        stage: None,
        worktree: None,
    };
    let tree = node(
        Kind::Space,
        "repo",
        vec![node(
            Kind::Thread,
            "t",
            vec![node(Kind::Agent, "a", vec![])],
        )],
    );
    let mut app = App::default();
    let open = HashSet::from([(tree.path.clone(), PathBuf::from("/t"))]);
    flatten(&tree, 0, &tree.path, "", &open, &mut app.rows);
    (app.side_inner, app.screen) = (Rect::new(1, 1, 98, 10), Rect::new(0, 0, 100, 20));
    let click = |b, x, y| MouseEvent {
        kind: MouseEventKind::Down(b),
        column: x,
        row: y,
        modifiers: KeyModifiers::NONE,
    };
    app.on_mouse(click(MouseButton::Right, 5, 1)); // the space row: no menu
    assert!(app.menu.is_none());
    app.on_mouse(click(MouseButton::Right, 5, 3)); // the agent row
    let m = app.menu.as_ref().unwrap();
    assert_eq!(
        (m.items.clone(), m.rect.y),
        (
            vec![Act::Terminate {
                thread: "t".into(),
                agent: "a".into()
            }],
            3
        )
    );
    app.on_mouse(click(MouseButton::Left, 50, 8)); // elsewhere: closes, opens nothing
    assert!(app.menu.is_none() && app.tabs.is_empty());
    app.on_mouse(click(MouseButton::Right, 5, 3));
    app.on_key(key(KeyCode::Esc, KeyModifiers::NONE));
    assert!(app.menu.is_none());
    app.on_mouse(click(MouseButton::Right, 97, 3)); // at the screen edge: the menu is shifted left
    assert_eq!(app.menu.as_ref().unwrap().rect.right(), 100);
    app.on_mouse(click(MouseButton::Right, 5, 2)); // the thread row: Delete / Archive
    let m = app.menu.as_ref().unwrap();
    assert_eq!(
        (m.items.clone(), m.rect.height),
        (vec![Act::Delete("t".into()), Act::Archive("t".into())], 2)
    );
    app.on_key(key(KeyCode::Down, KeyModifiers::NONE));
    app.on_key(key(KeyCode::Up, KeyModifiers::NONE));
    app.on_key(key(KeyCode::Enter, KeyModifiers::NONE)); // Delete asks first
    assert_eq!(
        (app.menu.is_none(), app.confirm_delete.as_deref()),
        (true, Some("t"))
    );
    app.on_key(key(KeyCode::Char('n'), KeyModifiers::NONE));
    assert!(app.confirm_delete.is_none());
}

#[test]
fn sidebar_click_keeps_focus() {
    let node = |kind, label: &str, children| Node {
        kind,
        label: label.into(),
        path: PathBuf::from(format!("/{label}")),
        children,
        stage: None,
        worktree: None,
    };
    let tree = node(
        Kind::Space,
        "repo",
        vec![node(Kind::Output, "nope.md", vec![])],
    );
    let mut app = App::default();
    flatten(&tree, 0, &tree.path, "", &HashSet::new(), &mut app.rows);
    app.side_inner = Rect::new(1, 1, 34, 10);
    let click = |x, y| MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: x,
        row: y,
        modifiers: KeyModifiers::NONE,
    };
    app.on_mouse(click(5, 2)); // the output row: opens a viewer tab, focus stays on the sidebar
    assert_eq!(
        (app.tabs[""].list[0].list.len(), app.focus_main),
        (1, false)
    );
    app.tabs.get_mut("").unwrap().list[0].body = Rect::new(40, 2, 40, 10);
    app.on_key(key(KeyCode::Char('k'), KeyModifiers::NONE));
    assert_eq!(app.sel, 0); // j/k moved the sidebar selection
    app.on_mouse(click(50, 5)); // the body
    assert!(app.focus_main);
}

#[test]
fn pause_button_toggles() {
    let mut app = App {
        pause_btns: vec![(Rect::new(28, 2, 7, 1), "t".into())],
        ..App::default()
    };
    let click = |x, y| MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: x,
        row: y,
        modifiers: KeyModifiers::NONE,
    };
    app.on_mouse(click(5, 2)); // beside the button: nothing
    assert!(app.paused.is_empty());
    app.on_mouse(click(30, 2));
    assert!(app.paused.contains("t"));
    app.on_mouse(click(30, 2));
    assert!(app.paused.is_empty());
}

#[test]
fn ctrl_t_acts_on_highlighted_thread() {
    let node = |kind, label: &str, children| Node {
        kind,
        label: label.into(),
        path: PathBuf::from(format!("/{label}")),
        children,
        stage: None,
        worktree: None,
    };
    let tree = node(Kind::Space, "repo", vec![node(Kind::Thread, "t", vec![])]);
    let mut app = App::default();
    flatten(&tree, 0, &tree.path, "", &HashSet::new(), &mut app.rows);
    let ctrl_t = || key(KeyCode::Char('t'), KeyModifiers::CONTROL);
    let ch = |c| key(KeyCode::Char(c), KeyModifiers::NONE);
    app.on_key(ctrl_t()); // the space row: no thread, so a flashes and acts on nothing
    app.on_key(ch('a'));
    assert_eq!(
        (app.thread_prefix, app.msg.as_ref().map(|m| m.0.as_str())),
        (false, Some("no thread highlighted"))
    );
    app.on_key(ctrl_t());
    app.on_key(ctrl_t()); // C-t C-t opens nothing
    assert!(!app.thread_prefix && app.newt.is_none() && app.confirm_delete.is_none());
    app.sel = 1; // the thread row
    app.on_key(ctrl_t());
    app.on_key(ch('d'));
    assert_eq!(app.confirm_delete.as_deref(), Some("t"));
    app.on_key(ch('n')); // declines the delete
    app.on_key(ctrl_t());
    app.on_key(ch('n'));
    assert_eq!(
        app.newt,
        Some(Newt {
            fields: ["/repo".into(), String::new(), String::new()],
            focus: 1,
            ..Default::default()
        })
    );
}

#[test]
fn master_row_pinned_above_spaces() {
    let node = |kind, label: &str, children| Node {
        kind,
        label: label.into(),
        path: PathBuf::from(format!("/{label}")),
        children,
        stage: None,
        worktree: None,
    };
    let mut app = App {
        tree: vec![
            node(Kind::Space, "a", vec![node(Kind::Thread, "t", vec![])]),
            node(Kind::Space, "b", vec![]),
        ],
        ..Default::default()
    };
    app.reflow();
    let rows: Vec<_> = app
        .rows
        .iter()
        .map(|r| (r.kind.clone(), r.label.as_str()))
        .collect();
    assert_eq!(
        rows,
        [
            (Kind::Master, "master"),
            (Kind::Space, "a"),
            (Kind::Thread, "t"),
            (Kind::Space, "b")
        ]
    );
    // The master banner is one line, then a blank line before the first space; the
    // shown space and thread are still the first real ones.
    let ys: Vec<_> = layout(&app.rows, 0, 20)
        .into_iter()
        .map(|(y, _)| y)
        .collect();
    assert_eq!(ys, [0, 2, 3, 5]);
    assert_eq!(
        (app.space.as_path(), app.thread.as_str()),
        (Path::new("/a"), "t")
    );
    // Ctrl-t never deletes or archives the master session.
    app.sel = 0;
    app.on_key(key(KeyCode::Char('t'), KeyModifiers::CONTROL));
    app.on_key(key(KeyCode::Char('d'), KeyModifiers::NONE));
    assert_eq!(app.confirm_delete, None);
}

#[test]
fn master_banner_fits_width_with_label() {
    for (w, label) in [(8, "NEW"), (14, "NEW THREAD"), (40, "NEW THREAD")] {
        let lines = brick_banner("● ", Style::new(), false, w);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].width(), w);
        let text: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains(&format!("● {label}")), "{text:?}");
        let span = lines[0].spans.iter().find(|s| s.content == label).unwrap();
        let m = span.style.add_modifier;
        assert!(m.contains(Modifier::BOLD | Modifier::ITALIC));
    }
}

#[test]
fn ctrl_t_m_opens_dir_focused() {
    let mut app = App {
        space: "/repo".into(),
        ..Default::default()
    };
    app.on_key(key(KeyCode::Char('t'), KeyModifiers::CONTROL));
    app.on_key(key(KeyCode::Char('m'), KeyModifiers::NONE));
    let n = app.newt.as_ref().unwrap();
    assert_eq!((n.fields[0].as_str(), n.focus), ("/repo", 0));
}

#[test]
fn plus_opens_new_thread_window() {
    let mut app = App {
        plus_btns: vec![(Rect::new(30, 1, 3, 1), PathBuf::from("/repo"))],
        ..Default::default()
    };
    app.on_mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: 31,
        row: 1,
        modifiers: KeyModifiers::NONE,
    });
    assert_eq!(
        app.newt,
        Some(Newt {
            fields: ["/repo".into(), String::new(), String::new()],
            focus: 1,
            ..Default::default()
        })
    );
    let typ = |app: &mut App, s: &str| {
        s.chars()
            .for_each(|c| app.on_key(key(KeyCode::Char(c), KeyModifiers::NONE)))
    };
    typ(&mut app, "ab");
    app.on_key(key(KeyCode::Backspace, KeyModifiers::NONE));
    app.on_key(key(KeyCode::Char('t'), KeyModifiers::ALT)); // no terminal opens behind the window
    assert_eq!(
        (
            app.newt.as_ref().unwrap().fields[1].as_str(),
            app.tabs.is_empty()
        ),
        ("a", true)
    );
    app.on_key(key(KeyCode::Backspace, KeyModifiers::NONE));
    typ(&mut app, "Bad!");
    app.newt.as_mut().unwrap().fields[0] = "/".into(); // a real dir, so the name is what fails
    app.on_key(key(KeyCode::Enter, KeyModifiers::NONE)); // name -> task
    assert_eq!(app.newt.as_ref().unwrap().focus, 2);
    app.on_key(key(KeyCode::Up, KeyModifiers::NONE));
    assert_eq!(app.newt.as_ref().unwrap().focus, 1);
    app.on_key(key(KeyCode::Tab, KeyModifiers::NONE));
    typ(&mut app, "x y");
    app.on_paste("p\nq");
    assert_eq!(app.newt.as_ref().unwrap().fields[2], "x yp q");
    app.on_key(key(KeyCode::Enter, KeyModifiers::NONE)); // a bad name keeps the window open with an error
    let n = app.newt.as_ref().unwrap();
    assert!(
        n.err.as_ref().unwrap().contains("bad thread name")
            && !n.planning
            && n.fields[2] == "x yp q"
            && n.focus == 1
    );
    app.on_key(key(KeyCode::Char('u'), KeyModifiers::CONTROL)); // clears the focused (name) field
    assert!(app.newt.as_ref().unwrap().fields[1].is_empty());
    // A bad dir fails first and focuses the dir field.
    app.on_key(key(KeyCode::Up, KeyModifiers::NONE));
    app.on_key(key(KeyCode::Char('u'), KeyModifiers::CONTROL));
    typ(&mut app, "/nonexistent-brigd-xyz");
    app.on_key(key(KeyCode::Down, KeyModifiers::NONE));
    typ(&mut app, "Bad!");
    app.on_key(key(KeyCode::Enter, KeyModifiers::NONE)); // name -> task
    app.on_key(key(KeyCode::Enter, KeyModifiers::NONE));
    let n = app.newt.as_ref().unwrap();
    assert!(
        n.err
            .as_deref()
            .unwrap()
            .contains("no such directory: /nonexistent-brigd-xyz")
            && n.focus == 0
    );
    app.on_key(key(KeyCode::Esc, KeyModifiers::NONE));
    assert!(app.newt.is_none());
    // "Bad!" can never be a thread, so n's delete touches nothing on disk.
    app.confirm_run = Some(("Bad!".into(), "/repo".into()));
    app.on_key(key(KeyCode::Char('x'), KeyModifiers::NONE));
    assert!(app.confirm_run.is_some());
    app.on_key(key(KeyCode::Char('n'), KeyModifiers::NONE));
    assert!(app.confirm_run.is_none());
}

#[test]
fn newt_asks_plan_review_go() {
    let name = format!("ask-t{}", std::process::id());
    let mk = |task: &str| App {
        newt: Some(Newt {
            fields: ["/".into(), name.clone(), task.into()],
            focus: 2,
            ..Default::default()
        }),
        ..Default::default()
    };
    let enter = |app: &mut App| app.on_key(key(KeyCode::Enter, KeyModifiers::NONE));
    // Enter on the task asks; nothing is reserved; Esc goes back to editing.
    let mut app = mk("do it");
    enter(&mut app);
    assert!(app.newt.as_ref().unwrap().ask);
    assert!(!crate::store::thread_dir(&name).exists());
    app.on_key(key(KeyCode::Esc, KeyModifiers::NONE));
    let n = app.newt.as_ref().unwrap();
    assert!(!n.ask && !n.planning && n.focus == 2);
    assert!(!crate::store::thread_dir(&name).exists());
    // A bad field fails before asking.
    let mut bad = mk("");
    enter(&mut bad);
    assert!(!bad.newt.as_ref().unwrap().ask && bad.newt.as_ref().unwrap().err.is_some());
    // Naming plan-review-go skips the question; 'y' adds the marker.
    assert_eq!(crate::with_prg("do it"), "do it\n\nUse plan-review-go.");
    assert!(crate::mentions_prg(&crate::with_prg("x")) && !crate::mentions_prg("do it"));
}

#[test]
fn chunk_by_width() {
    assert_eq!(chunk("abcde", 2), ["ab", "cd", "e"]);
    assert_eq!(chunk("", 2), [""]);
    assert_eq!(chunk("日本語", 4), ["日本", "語"]); // wide chars are 2 columns
    assert_eq!(chunk("a\nb", 4), ["a", "b"]);
}

#[test]
fn newt_planning_state() {
    let mut app = App {
        newt: Some(Newt {
            fields: ["/r".into(), "n".into(), "t".into()],
            planning: true,
            ..Default::default()
        }),
        ..Default::default()
    };
    app.on_key(key(KeyCode::Char('x'), KeyModifiers::NONE)); // read-only
    app.on_key(key(KeyCode::Esc, KeyModifiers::NONE)); // hides, keeps planning
    assert!(app.newt.as_ref().unwrap().hidden);
    app.on_bg(Bg::Failed("n".into(), "boom".into())); // reopens, editable, fields kept
    let n = app.newt.as_ref().unwrap();
    assert!(
        !n.hidden
            && !n.planning
            && n.err.as_deref() == Some("boom")
            && (n.fields[1].as_str(), n.fields[2].as_str()) == ("n", "t")
    );
}

#[test]
fn newt_draws_on_tiny_terminal() {
    let mut app = App {
        newt: Some(Newt {
            fields: ["/r".into(), "n".into(), "日本語".repeat(40)],
            err: Some("a\nb".into()),
            ..Default::default()
        }),
        ..Default::default()
    };
    for (w, h) in [(1, 1), (10, 3), (80, 4), (80, 24)] {
        let mut t = Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap();
        t.draw(|f| app.draw(f)).unwrap();
    }
}

#[test]
fn drag_copies_selection() {
    let mut app = App {
        buf: Buffer::with_lines([
            "side|hello world  ",
            "side|second line  ",
            "side|third        ",
        ]),
        side: Rect::new(0, 0, 5, 3),
        ..App::default()
    };
    app.focused_pane().body = Rect::new(5, 0, 13, 3);
    let ev = |kind, x, y| MouseEvent {
        kind,
        column: x,
        row: y,
        modifiers: KeyModifiers::NONE,
    };
    let (down, drag, up) = (
        MouseEventKind::Down(MouseButton::Left),
        MouseEventKind::Drag(MouseButton::Left),
        MouseEventKind::Up(MouseButton::Left),
    );
    app.on_mouse(ev(down, 11, 0));
    app.on_mouse(ev(drag, 10, 1));
    app.on_mouse(ev(up, 10, 1));
    assert_eq!(app.clip.take().as_deref(), Some("world\nsecond"));
    app.on_mouse(ev(down, 6, 2)); // backwards past the body's left edge: clamped
    app.on_mouse(ev(drag, 0, 2));
    app.on_mouse(ev(up, 0, 2));
    assert_eq!(app.clip.take().as_deref(), Some("th"));
    app.on_mouse(ev(down, 7, 1)); // a plain click copies nothing
    app.on_mouse(ev(up, 7, 1));
    assert!(app.clip.is_none());
    assert_eq!(
        (base64(b"Man"), base64(b"Ma"), base64(b"M")),
        ("TWFu".into(), "TWE=".into(), "TQ==".into())
    );
}

#[test]
fn divider_drag_resizes() {
    let mut app = App::default();
    (app.side, app.divider) = (Rect::new(0, 0, 40, 5), Rect::new(40, 0, 1, 5));
    app.focused_pane().body = Rect::new(41, 0, 40, 5);
    let ev = |kind, x| MouseEvent {
        kind,
        column: x,
        row: 1,
        modifiers: KeyModifiers::NONE,
    };
    let (down, drag, up) = (
        MouseEventKind::Down(MouseButton::Left),
        MouseEventKind::Drag(MouseButton::Left),
        MouseEventKind::Up(MouseButton::Left),
    );
    app.on_mouse(ev(down, 40));
    app.on_mouse(ev(drag, 25));
    assert_eq!(app.side_w, Some(25));
    assert!(app.drag.is_none()); // no text selection
    app.on_mouse(ev(drag, 70)); // past SIDE_W: stops there
    assert_eq!(app.side_w, Some(SIDE_W));
    app.on_mouse(ev(drag, 2)); // and never below the minimum
    assert_eq!(app.side_w, Some(MIN_SIDE_W));
    app.on_mouse(ev(up, 2));
    app.on_mouse(ev(drag, 30)); // released: a stray drag moves nothing
    assert_eq!(app.side_w, Some(MIN_SIDE_W));
}

#[test]
fn sidebar_drag_selects_nothing() {
    let mut app = App {
        buf: Buffer::with_lines(["side|hello world  ", "side|second line  "]),
        side: Rect::new(0, 0, 5, 2),
        ..App::default()
    };
    app.focused_pane().body = Rect::new(5, 0, 13, 2);
    let ev = |kind, x, y| MouseEvent {
        kind,
        column: x,
        row: y,
        modifiers: KeyModifiers::NONE,
    };
    app.on_mouse(ev(MouseEventKind::Down(MouseButton::Left), 2, 0));
    app.on_mouse(ev(MouseEventKind::Drag(MouseButton::Left), 10, 1));
    app.on_mouse(ev(MouseEventKind::Up(MouseButton::Left), 10, 1));
    assert!(app.drag.is_none() && app.clip.is_none());
}

#[test]
fn auto_archive_keeps_threads_in_use() {
    let root = std::env::temp_dir().join(format!("brigd-auto-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let old = SystemTime::now() - Duration::from_secs(8 * 86400);
    for t in ["idle", "alive", "tabbed"] {
        let f = root.join("threads").join(t).join("state.json");
        fs::create_dir_all(f.parent().unwrap()).unwrap();
        fs::write(&f, "x").unwrap();
        fs::File::options()
            .write(true)
            .open(&f)
            .unwrap()
            .set_modified(old)
            .unwrap();
    }
    // A live pid under the thread (this test process) keeps it.
    let pid = root.join("threads/alive/pid");
    fs::write(&pid, std::process::id().to_string()).unwrap();
    fs::File::options()
        .write(true)
        .open(&pid)
        .unwrap()
        .set_modified(old)
        .unwrap();
    let mut app = App::default();
    app.tabs.insert(
        "tabbed".into(),
        one_pane(vec![Tab::View {
            path: PathBuf::new(),
            title: String::new(),
            text: String::new(),
            scroll: 0,
            rel: None,
        }]),
    );
    let stale = app.stale_threads(&root, &root, crate::store::now());
    assert_eq!(stale, ["idle"]);
    // The open tab was touched: its week restarts.
    assert!(root.join("threads/tabbed/seen").exists());
    assert!(crate::store::last_active(&root.join("threads/tabbed")) >= crate::store::now() - 5);
    let _ = fs::remove_dir_all(&root);
}
