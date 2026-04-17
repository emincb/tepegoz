# Handoff — retired 2026-04-17

This doc existed to carry in-flight thinking between two separate Claude
Code sessions (CTO planner + implementing engineer) that shared only the
user as a relay. That architecture is retired: a single Claude session
now runs planning + implementation + verification + commit + push
end-to-end. User involvement gates on product decisions only.

**Where the prior responsibilities moved:**

- **Authoritative current state** → `docs/STATUS.md`.
- **Locked architectural commitments** → `docs/DECISIONS.md`.
- **Phase plans** → `docs/ROADMAP.md`.
- **Active bugs** → `docs/ISSUES.md`.
- **Operating model + escalation criteria** → `CLAUDE.md` §"Operating
  model — single-session autonomy".
- **Cross-conversation memory** (what's in the assistant's head that
  hasn't crystallized into docs yet) → the `memory/` auto-memory store.

Prior content preserved in git history. Retrieve via
`git log --follow --all -- docs/HANDOFF.md` or
`git show <commit>:docs/HANDOFF.md` — last substantive version is at
`672b199` (Phase 6 close era).
