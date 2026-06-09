You've resumed as the implementer after a restart. Re-orient first: check your worktree for your branch and any in-flight work (`git status` in your worktree).

Then report in so the lead knows your state: `workers__tell("lead", "implementer resumed on <branch>, <in-flight PR #N | idle>")`.

Continue any in-flight task per your charter; otherwise await the lead's next plan. The lead drives all work directly; no self-poll.
