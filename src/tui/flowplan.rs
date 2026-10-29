//! FLOWPLAN and flow.md rendering: the step tree of a thread's flow, and mermaid flowcharts drawn the same way.

use super::theme::{pal, stage_color};
use crate::store::Status;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use std::ops::Range;

/// Braille spinner for running steps; the frame comes from the clock (run redraws every ≤40ms).
pub(super) const SPIN: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Words a collapsed FLOWPLAN task shows before its "...".
const TASK_WORDS: usize = 10;

/// The flow as an npx-style step tree ending at crate::flow::LOG_MARK; also the plain FLOWPLAN
/// text (crate::flow::flow_text). `status(agent)`: pending / running / blocked / done / failed.
/// A task shows its first TASK_WORDS words and a "..." unless `expanded(agent)`. Each task
/// line pushes (line index, clickable columns, agent) to `hits`: the "..." when collapsed,
/// the whole line when expanded.
pub fn plan_lines(
    flow: &crate::store::Flow,
    status: impl Fn(&str) -> Status,
    expanded: impl Fn(&str) -> bool,
    hits: &mut Vec<(usize, Range<u16>, String)>,
    frame: usize,
    width: usize,
) -> Vec<Line<'static>> {
    let mut out = goal_lines(&flow.goal, width);
    let w = flow
        .stages
        .iter()
        .flatten()
        .map(|a| a.name.len())
        .max()
        .unwrap_or(0);
    for (i, stage) in flow.stages.iter().enumerate() {
        let c = stage_color(Some(i));
        let sts: Vec<Status> = stage.iter().map(|a| status(&a.name)).collect();
        let st = if sts.iter().all(|s| *s == Status::Done) {
            Status::Done
        } else if sts.contains(&Status::Failed) {
            Status::Failed
        } else if sts
            .iter()
            .any(|s| matches!(s, Status::Running | Status::Blocked))
        {
            Status::Running
        } else {
            Status::Pending
        };
        let n = if stage.len() == 1 {
            "1 agent".to_string()
        } else {
            format!("{} parallel", stage.len())
        };
        out.push(Line::from(guide("│")));
        out.push(Line::from(vec![
            step(&st, frame),
            Span::raw("  "),
            Span::styled(
                format!("stage {}", i + 1),
                Style::new().fg(c).add_modifier(Modifier::BOLD),
            ),
            guide(&format!(" · {n}")),
        ]));
        for (j, (a, st)) in stage.iter().zip(&sts).enumerate() {
            let last = j + 1 == stage.len();
            let wt = a
                .worktree
                .as_deref()
                .filter(|w| !w.is_empty())
                .map_or("main".into(), |w| format!("wt {w}"));
            let meta = format!("{} · {} · {wt}", a.model, a.effort);
            out.push(Line::from(vec![
                guide(if last { "│  └─ " } else { "│  ├─ " }),
                step(st, frame),
                Span::styled(format!(" {:<w$}  ", a.name), Style::new().fg(c)),
                guide(&meta),
            ]));
            task_lines(&mut out, hits, a, last, c, &expanded, width);
        }
    }
    out.push(Line::from(guide("│")));
    out.push(Line::from(guide(crate::flow::LOG_MARK)));
    out
}

/// Tree guide characters, dimmed.
fn guide(s: &str) -> Span<'static> {
    Span::styled(s.to_string(), Style::new().fg(pal().overlay0))
}

/// The flow's goal, word-wrapped after a ◆.
fn goal_lines(goal: &str, width: usize) -> Vec<Line<'static>> {
    let mut out = vec![];
    for (i, l) in wrap(goal, width.saturating_sub(3).max(20))
        .into_iter()
        .enumerate()
    {
        let lead = if i == 0 {
            Span::styled("◆  ", Style::new().fg(pal().mauve))
        } else {
            guide("│  ")
        };
        out.push(Line::from(vec![
            lead,
            Span::styled(l, Style::new().fg(pal().text).add_modifier(Modifier::BOLD)),
        ]));
    }
    out
}

/// Agent `a`'s task under its row (`last`: the stage's last agent), pushed to `out` with its
/// clickable spans in `hits` (see plan_lines).
fn task_lines(
    out: &mut Vec<Line<'static>>,
    hits: &mut Vec<(usize, Range<u16>, String)>,
    a: &crate::store::Agent,
    last: bool,
    c: Color,
    expanded: &impl Fn(&str) -> bool,
    width: usize,
) {
    let dim = Style::new().fg(pal().overlay0);
    let tw = width.saturating_sub(8).max(20);
    let lead = if last { "│       " } else { "│  │    " };
    let words: Vec<&str> = a.task.split_whitespace().collect();
    if words.len() > TASK_WORDS && !expanded(&a.name) {
        let rows = wrap(&words[..TASK_WORDS].join(" "), tw);
        let n = rows.len();
        for (k, row) in rows.into_iter().enumerate() {
            let mut l = vec![guide(lead)];
            if k + 1 == n {
                let x = (lead.chars().count() + row.chars().count() + 1) as u16;
                hits.push((out.len(), x..x + 3, a.name.clone()));
                l.push(Span::styled(row + " ", dim.add_modifier(Modifier::ITALIC)));
                l.push(Span::styled(
                    "...",
                    Style::new().fg(c).add_modifier(Modifier::BOLD),
                ));
            } else {
                l.push(Span::styled(row, dim.add_modifier(Modifier::ITALIC)));
            }
            out.push(Line::from(l));
        }
        return;
    }
    // Blank task lines are dropped, like wrap() drops them in the goal.
    for l in a.task.lines().filter(|l| !l.trim().is_empty()) {
        for row in wrap(l, tw) {
            if words.len() > TASK_WORDS {
                hits.push((out.len(), 0..u16::MAX, a.name.clone()));
            }
            out.push(Line::from(vec![
                guide(lead),
                Span::styled(row, dim.add_modifier(Modifier::ITALIC)),
            ]));
        }
    }
}

type Nodes = Vec<(String, String)>;
type Edges = Vec<(usize, usize, String)>;

/// A mermaid flowchart (flow.md): its nodes as (id, label) and edges as (from, to, label), in source order.
/// None unless it opens with `flowchart`/`graph` and has a node. Handles `A[x] --> B{y}`, chains and `-->|label|`.
fn parse_flow(src: &str) -> Option<(Nodes, Edges)> {
    let body = src
        .split("```mermaid")
        .nth(1)
        .map_or(src, |s| s.split("```").next().unwrap_or(s));
    let mut lines = body
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with("%%"));
    let head = lines.next()?;
    if !(head.starts_with("flowchart") || head.starts_with("graph")) {
        return None;
    }
    let (mut nodes, mut edges) = (Vec::<(String, String)>::new(), vec![]);
    let mut node = |tok: &str| -> Option<usize> {
        let tok = tok.trim();
        let i = tok.find(['[', '(', '{', '>']).unwrap_or(tok.len());
        let id = tok[..i].trim();
        if id.is_empty() || id.contains(char::is_whitespace) {
            return None;
        }
        let label = tok[i..].trim_matches(|c| "[](){}>\"/\\ ".contains(c));
        let k = nodes.iter().position(|n| n.0 == id).unwrap_or_else(|| {
            nodes.push((id.into(), id.into()));
            nodes.len() - 1
        });
        if !label.is_empty() {
            nodes[k].1 = label.into();
        }
        Some(k)
    };
    for l in lines {
        if [
            "style ",
            "classDef ",
            "class ",
            "click ",
            "linkStyle ",
            "subgraph",
            "end",
            "direction ",
        ]
        .iter()
        .any(|k| l.starts_with(k))
        {
            continue;
        }
        let mut parts = l.split("-->");
        let mut from: Vec<usize> = parts
            .next()?
            .split('&')
            .map(&mut node)
            .collect::<Option<_>>()?;
        for p in parts {
            let (label, rest) = match p
                .trim_start()
                .strip_prefix('|')
                .and_then(|r| r.split_once('|'))
            {
                Some((lab, rest)) => (lab.trim(), rest),
                None => ("", p),
            };
            let to: Vec<usize> = rest.split('&').map(&mut node).collect::<Option<_>>()?;
            for &f in &from {
                edges.extend(to.iter().map(|&t| (f, t, label.to_string())));
            }
            from = to;
        }
    }
    (!nodes.is_empty()).then_some((nodes, edges))
}

/// Nodes grouped into steps: a node sits one step below its deepest parent. A loop's
/// back edge is dropped: when none is left without parents the first unplaced node starts.
fn flow_steps(n: usize, edges: &[(usize, usize, String)]) -> Vec<Vec<usize>> {
    let mut indeg = vec![0; n];
    for e in edges.iter().filter(|e| e.0 != e.1) {
        indeg[e.1] += 1;
    }
    let (mut depth, mut placed, mut queue) = (vec![0; n], vec![false; n], vec![]);
    for _ in 0..n {
        let next = queue
            .pop()
            .or_else(|| (0..n).find(|&v| !placed[v] && indeg[v] == 0));
        let Some(u) = next.or_else(|| (0..n).find(|&v| !placed[v])) else {
            break;
        };
        placed[u] = true;
        for e in edges.iter().filter(|e| e.0 == u && !placed[e.1]) {
            depth[e.1] = depth[e.1].max(depth[u] + 1);
            indeg[e.1] -= 1;
            if indeg[e.1] == 0 && !queue.contains(&e.1) {
                queue.push(e.1);
            }
        }
    }
    let mut steps = vec![vec![]; n];
    for v in 0..n {
        steps[depth[v]].push(v);
    }
    steps.retain(|s| !s.is_empty());
    steps
}

/// flow.md's mermaid flowchart as a FLOWPLAN-style step tree; None (→ raw markdown) if it won't parse.
/// A node whose parents aren't exactly the step above names them (`← a, b`), so branches stay readable.
/// ponytail: O(nodes × edges) per frame; fine for the ≤10-node flows agents are asked for.
pub(super) fn flow_lines(text: &str) -> Option<Vec<Line<'static>>> {
    let (nodes, edges) = parse_flow(text)?;
    let steps = flow_steps(nodes.len(), &edges);
    let dim = Style::new().fg(pal().overlay0);
    let mut out = vec![];
    for (i, s) in steps.iter().enumerate() {
        if i > 0 {
            out.push(Line::styled("│", dim));
        }
        let mut prev = if i > 0 { steps[i - 1].clone() } else { vec![] };
        prev.sort();
        for (j, &v) in s.iter().enumerate() {
            let lead = match (s.len(), j + 1 == s.len()) {
                (1, _) => "",
                (_, true) => "└─ ",
                _ => "├─ ",
            };
            let ins: Vec<_> = edges.iter().filter(|e| e.1 == v && e.0 != v).collect();
            let mut ps: Vec<usize> = ins.iter().map(|e| e.0).collect();
            ps.sort();
            ps.dedup();
            let mut l = vec![
                Span::styled(lead, dim),
                step(&Status::Done, 0),
                Span::raw("  "),
            ];
            l.push(Span::styled(
                nodes[v].1.clone(),
                Style::new().fg(stage_color(Some(i))),
            ));
            if !ps.is_empty() && ps != prev {
                let names: Vec<&str> = ps.iter().map(|&p| nodes[p].1.as_str()).collect();
                l.push(Span::styled(format!("  ← {}", names.join(", ")), dim));
            }
            if let Some(e) = ins.iter().find(|e| !e.2.is_empty()) {
                l.push(Span::styled(format!("  · {}", e.2), dim));
            }
            out.push(Line::from(l));
        }
    }
    Some(out)
}

/// A step's glyph: pending ◇, running spinner, blocked !, done ✔, failed ✖.
fn step(st: &Status, frame: usize) -> Span<'static> {
    match st {
        Status::Done => Span::styled("✔", Style::new().fg(pal().green)),
        Status::Failed => Span::styled("✖", Style::new().fg(pal().red)),
        Status::Blocked => {
            Span::styled("!", Style::new().fg(pal().red).add_modifier(Modifier::BOLD))
        }
        Status::Running => Span::styled(SPIN[frame % SPIN.len()], Style::new().fg(pal().sky)),
        _ => Span::styled("◇", Style::new().fg(pal().overlay0)),
    }
}

/// Word-wraps `s` to `w` columns (a longer word gets a line of its own).
fn wrap(s: &str, w: usize) -> Vec<String> {
    let mut out = vec![String::new()];
    for word in s.split_whitespace() {
        let cur = out.last().map_or(0, |l| l.chars().count());
        if cur > 0 && cur + 1 + word.chars().count() > w {
            out.push(String::new());
        }
        let l = out.last_mut().unwrap();
        if !l.is_empty() {
            l.push(' ');
        }
        l.push_str(word);
    }
    out
}
