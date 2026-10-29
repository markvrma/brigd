---
name: plan-review-go
description: For any non-trivial task, first draft a plan, have ponytail review it, revise per feedback, build a todo list, then ask the user before starting work. Use when the user says "plan-review-go", "/plan-review-go", or asks to plan-then-execute a task with review.
user-invocable: true
argument-hint: "[task description]"
---

# Plan → Review → Go

Turn a task request into a reviewed, approved plan before writing any code.

Task: `$ARGUMENTS`

## Running inside brigd

If the prompt starts with `# brigd task:` and has a `## Flow map` section naming a
flowmap.json path, you are a brigd stage-1 agent. Run steps 0-5 as usual. Step 6
changes: after the user approves, do NOT implement. Instead:

a. Read the installed brigd-plan skill (`~/.claude/skills/brigd-plan/SKILL.md` or `~/.agents/skills/brigd-plan/SKILL.md`; in the brigd repo, `skills/brigd-plan/SKILL.md`) for its rules.
b. Append stages 2..N to the flowmap.json that implement the approved plan. Keep
   every existing stage unchanged.
c. Re-read the file; check it parses as valid JSON matching the schema: names match
   `^[a-z][a-z0-9_-]{0,18}$`, are unique and not `log`; at most one editing agent
   per worktree per stage; real file paths.
d. Write the approved plan and the list of added stages to the output.md path given
   in the prompt, ending with `## Files changed` listing the flowmap.json.

If the user stops at the permission step, write output.md with the plan and add no
stages.

Run these steps in order. Do not skip ahead.

## 0. Discover skills

Run the `find-skills` skill first. Surface any installed or installable skills
relevant to this task — use them when drafting the plan (step 1) and when
reviewing (step 2). Fold relevant skills into the plan's steps.

## 1. Draft plan

Draft a concise implementation plan for the task:
- Goal (one line)
- Files to touch
- Steps in order
- Anything skipped and why

Keep it tight. No code yet.

## 2. Ponytail review

Use a subagent if your agent has them, else do it yourself, to run the over-engineering review — have it
invoke the `ponytail:ponytail-review` skill (or review through the ponytail
lens) on the drafted plan. Pass it the full plan text in the prompt. Ask it:
what in this plan can be deleted, simplified, or replaced with stdlib/native?
Capture the findings the subagent returns.

## 3. Revise

If ponytail flagged changes, revise the plan to fold them in. If it flagged
nothing, note "no changes" and keep the plan as-is. Show the revised plan.

## 4. Grill the revised plan

Use a subagent if your agent has them, else do it yourself, to run the grilling — have it invoke the
`grill-me` skill (a `/grilling` session) on the plan as revised in step 3 (the
version that already folds in ponytail's edits). Pass it the full revised plan
in the prompt, and instruct it to return the list of questions it generates
rather than interviewing itself.

- Show the user the full list of questions the subagent returns.
- Answer each yourself where the answer is obvious from the task, the code, or
  the plan itself — state your answer inline.
- For any question whose answer is NOT obvious, stop and ask the user before
  proceeding. Do not guess.
- Fold the answers (yours + the user's) back into the plan. Show the final
  plan.

## 5. Todo list

Turn the final plan into a todo list, one item per concrete step.

## 6. Ask permission

Stop. Ask the user: "Plan ready. Start work?" — do NOT begin implementation
until the user confirms. Once confirmed, work the todo list (inside brigd:
append stages instead, see above).
