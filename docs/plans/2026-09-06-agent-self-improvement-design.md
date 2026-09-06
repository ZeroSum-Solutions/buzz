# Agent self-improvement: work logs, retro, curator, prompt PRs

Ticket: T18 · `feat/agent-curator` (Buzz side is small; most of this is prompts and one workflow).
Depends on T16 and T17 (there is no failure evidence to learn from until they land).
Status: design, 2026-09-06. Audited by GPT-5.6 Sol.

## What "evolution like Hermes" means here

Hermes has three parts: agents save skills learned from sessions, a curator model reviews those skills on a schedule (prunes, merges, archives, never deletes), and an insights command reports usage. Nothing changes itself without a record and a way back.

| Hermes | Buzz equivalent in this design |
|---|---|
| learned skills | `~/.buzz/GUIDES/LESSONS_<AGENT>.md`, written by the agent from its own work logs and failures |
| curator | a weekly Curator run that consolidates lessons and proposes prompt edits as a pull request |
| insights | `buzz agents health` (T17) |
| journey / memory graph | the existing engram graph on the agent profile |

Prompts never edit themselves. Every change to an agent's behaviour is a diff that Devin merges.

## Phase 0, now: the work-log rule (prompt only)

Add house rule 8 to `~/projects/clients/broken-english/buzz/agent-prompts/_house-rules.md`:

> 8. Work log. When a task ends, write `~/.buzz/WORK_LOGS/YYYY-MM-DD-<agent>-<slug>.md` with frontmatter (title, tags, status, created) and five short sections: Asked, Done, Files, Failed or blocked, Would change next time. One file per task, under 40 lines. This is the only place to reflect; never post reflections in the channel.

The nest already defines `WORK_LOGS/`; it is empty because no prompt asked for it. Cost: one file write per task. This is a PR to the Broken English repo and a prompt reload in the desktop.

## Phase 1, after T17: weekly retro

A scheduled run (a Buzz workflow with `on: schedule, interval: 7d` once Workflows is on, otherwise a launchd job that runs the CLI) posts one message in a private ops thread: "Curator: weekly retro for <week>". The Curator is a new managed agent on Sonnet 5 with read access to the nest and the Broken English repo and no channel write beyond the ops thread. Its prompt:

1. Run `buzz agents health --since 7d --json` and read `~/.buzz/WORK_LOGS/` for the week.
2. For each agent, write or update `GUIDES/LESSONS_<AGENT>.md`: what failed, why, what the agent should do differently. Every lesson cites a ledger batch id or a work-log file.
3. Where a lesson implies a prompt change, edit the agent's prompt file in a branch of the Broken English repo and open a PR titled "curator: <agent> — <one line>", body listing the evidence. House rules are out of bounds for the Curator.
4. Post the PR links and a five-line summary in the ops thread. Stop.

Budget: one run a week, 30 minutes hard cap, 60 messages of nest reading. Devin merges or closes each PR. When a PR merges, the desktop's prompt-source reload picks up the new prompt on the next agent start.

## Phase 2, later: agent-level retro

Each agent reads its own `LESSONS` file at session start (one line in the prompt) so a lesson takes effect before the prompt PR is merged. Only after Phase 1 has run four times and the lessons have proved useful.

## Guardrails

- The Curator never edits a running prompt, a house rule, a channel, or a nest file outside `GUIDES/LESSONS_*`.
- A lesson without a citation is deleted by the next run.
- The Curator's own failures show in the Health tab like any agent's.
- The weekly cadence is fixed. Eleven nightly model runs would cost more than the failures they prevent.

## Tests

Phase 0: none beyond the prompt PR review. Phase 1: a dry-run mode for the Curator prompt on a fixture week (three failed turns, two work logs) that must produce one lessons file with two cited lessons and one prompt PR; checked by Critic before the first live run.

## Out of scope

Automatic prompt application, per-turn self-critique, any model fine-tuning.
