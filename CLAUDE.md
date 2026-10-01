# Project Execution Protocol — y4p-rs

This document complements the global protocol (`~/.claude/CLAUDE.md`) and defines repository-specific invariants, verification tooling, and context management protocols for `y4p-rs`.

---

## 1. Project Invariants & Constraints

- **Language Standard**:
  - All code comments, commit messages, PR descriptions, and technical documentation MUST use **British English** (e.g. sanitise, normalise, behaviour, prioritise, cancelled).
- **Zero External Dependencies**:
  - Do NOT add new crates to `Cargo.toml`. Rely strictly on the standard library (`std`), existing dependencies, and slice operations.
- **Panic-Free & Strict Clippy**:
  - No `unwrap()`, `expect()`, or `panic!()` in production code.
  - Zero warnings under `cargo clippy --all-targets -- -D warnings`.
- **Storage Subsystem Invariance**:
  - The logic, schema, queries, and interface of `src/storage/` must remain unchanged (0 logic changes). Pure formatting changes applied by `cargo fmt` are permitted.
- **Trialspace Path**:
  - Work isolated within `~/Documents/Projects/Trialspace/CLAUDE/y4p-rs/`. Never write unverified changes directly to the main production repository.

---

## 2. Verification Tooling (`scripts/check.sh`)

Instead of running verbose verification commands manually, use the automated check script:
```bash
bash scripts/check.sh
```
This script executes:
1. `cargo fmt --all -- --check`
2. `cargo test --all-targets`
3. `cargo clippy --all-targets -- -D warnings`

All three checks must pass with zero errors and zero warnings before presenting work for review.

---

## 3. Working Memory Protocol (`status.md`)

`status.md` serves as **Claude's personal working memory (cognitive scratchpad)** to ensure seamless recovery across `/clear` resets. It is not an external report, but self-directed state persistence.

### Management Rules:
1. **Hot Context (Active & Timely)**:
   - Provide **rich details** on: current active tasks, recent architectural decisions, edge cases encountered, unmerged diffs, and blockers/cautions.
2. **Cold Context (Completed / Roadmap)**:
   - **Minimise and summarise** past completed tasks, merged PRs, and future roadmap items into concise bullet points to prevent token bloat.
3. **Latest Handoff Section (At the very bottom)**:
   - Conclude `status.md` with a concise summary titled `## Latest Handoff for Human & Gemini`:
     - Summary of changes made in the latest iteration
     - Verification status (`check.sh` result)
     - Key questions or decisions requiring human approval

---

## 4. Session & Token Lifecycle (/clear Protocol)

- **Post-Milestone Refresh**:
  - Expect the human to run `/clear` at major milestone boundaries (version completions, major refactors).
- **Fast Bootstrap**:
  - Upon starting a session (or after `/clear`), read this `CLAUDE.md` and `status.md` to immediately restore project state and active context without relying on chat history.
