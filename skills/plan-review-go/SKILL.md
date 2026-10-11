---
name: plan-review-go
description: For any non-trivial task, first draft a plan, have ponytail review it, revise per feedback, build a todo list, then ask the user before starting work. Use when the user says "plan-review-go", "/plan-review-go", or asks to plan-then-execute a task with review.
user-invocable: true
argument-hint: "[task description]"
---

# Plan → Review → Go

Turn a task request into a reviewed, approved plan before writing any code.

Task: `$ARGUMENTS`

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

Spawn a subagent (Agent tool) to run the over-engineering review — have it
invoke the `ponytail:ponytail-review` skill (or review through the ponytail
lens) on the drafted plan. Pass it the full plan text in the prompt. Ask it:
what in this plan can be deleted, simplified, or replaced with stdlib/native?
Capture the findings the subagent returns.

## 3. Revise

If ponytail flagged changes, revise the plan to fold them in. If it flagged
nothing, note "no changes" and keep the plan as-is. Show the revised plan.

## 4. Grill the revised plan

Spawn a subagent (Agent tool) to run the grilling — have it invoke the
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

Turn the final plan into a todo list (TodoWrite), one item per concrete step.

## 6. Ask permission

Stop. Ask the user: "Plan ready. Start work?" — do NOT begin implementation
until the user confirms. Once confirmed, work the todo list.
