# Project Execution Protocol — y4p-rs

This document complements the global protocol (`~/.claude/CLAUDE.md`) and defines repository-specific invariants, verification tooling, and development cadences for `y4p-rs`.

**ABSOLUTE CORE CONFIRMATION: The global protocol (`~/.claude/CLAUDE.md`) possesses supreme authority. Agents MUST NOT defuse, bypass, or violate its safety perimeters (language separation, grounding, protected branches, pre-commit review, human-in-the-loop gates) under any circumstances.**

> **LIVING DOCUMENT PRINCIPLE**:
> This document is not a static dogma. It is an evolving document to be cultivated and refined alongside project progression and human direction.

---

## ⚠️ ABSOLUTE SAFETY RULES (NON-NEGOTIABLE)

1. **NO DIRECT MODIFICATIONS TO PRODUCTION REPOSITORY**:
   - You MUST NOT create, edit, modify, or delete any files in the production workspace:
     `PRODUCTION (READ-ONLY FOR YOU): /home/yukkkk1/Documents/Projects/Personal/yukkkk1-lab/y4p-rs/`
   - ALL code changes, file creations, and command executions MUST be restricted strictly to your Trialspace:
     `YOUR ISOLATED WORKSPACE: /home/yukkkk1/Documents/Projects/Trialspace/CLAUDE/y4p-rs/`
   - Always verify that file paths passed to editing/writing tools reside inside the Trialspace path.

2. **PROTECTED BRANCH AND AUTONOMOUS VCS RESTRICTIONS**:
   - Direct pushes or commits to protected branches (`main`) are strictly forbidden.
   - Any branch creation or switching in the production repository is handled by the human supervisor or Gemini.

---

## 1. Project Invariants & Pragmatic Exceptions

> **PRAGMATIC ATTITUDE**:
> The project invariants below serve as strict defaults against chaos. However, they must not be dogmatised.
> **Any invariant may be granted an exception or relaxation if logical facts, mathematical superiority, or inevitability are demonstrated and approved by the human supervisor.** Propose exceptions proactively with logical evidence.

- **Language Standard**:
  - All code comments, commit messages, PR descriptions, and technical documentation MUST use **British English** (e.g. sanitise, normalise, behaviour, prioritise, cancelled).
- **Zero External Dependencies**:
  - Do NOT add new crates to `Cargo.toml`. Rely strictly on the standard library (`std`), existing dependencies, and slice operations.
- **Panic-Free & Strict Clippy**:
  - No `unwrap()`, `expect()`, or `panic!()` in production code.
  - Zero warnings under `cargo clippy --all-targets -- -D warnings`.
- **Storage Subsystem Invariance**:
  - The logic, schema, queries, and interface of `src/storage/` must remain completely untouched (0 lines changed).

---

## 2. Release Cadence & Documentation Cadence

1. **Version Lifecycle**:
   - **`.N.1` - `.N.3` + `.N.5`**: Feature additions, specification completion, and bug fixes.
   - **`.N.4` + `.N.6` - `.N.9`**: Optimisation, refactoring, security hardening, and code/documentation synchronization.
   - (If slots are exhausted, increment to `.N.10` etc. May be skipped if major upgrade prep completes early at `.N.6`.)
2. **Documentation Alignment Timing**:
   - Conducted strictly at **`.N.9` or the highest minor version preceding a major upgrade**.
   - Ensures systematic architectural documentation distillation before every major release.
3. **Two-Stage Documentation & Code Comments**:
   - **Active Development**: Detailed architectural and "Why" comments within code are encouraged.
   - **Distillation Phase**: Detailed context is distilled and promoted into `docs/`. Code comments are refined into concise invariants and "Why".
   - **Exception**: Comments preceding unit tests and critical/large functions may retain rich explanations regardless of documentation duplication.

---

## 3. Verification Tooling (`scripts/check.sh`)

Instead of running verbose verification commands manually, use the automated check script inside your Trialspace:
```bash
bash scripts/check.sh
```
This script executes:
1. `cargo test --all-targets`
2. `cargo clippy --all-targets -- -D warnings`

Both checks must pass with zero errors and zero warnings before presenting work for review.

---

## 4. Working Memory Protocol (`status.md`)

`status.md` serves as **Claude's personal working memory (cognitive scratchpad)** to ensure seamless recovery across `/clear` resets.

### Management Rules:
1. **Simplified Reporting & Pointer Discipline**:
   - Keep `status.md` focused on task status, blockers, and handoffs. Avoid duplicating lengthy architectural rationale; use file/line reference pointers instead (e.g. `[src/xxx.rs:L10-L25]`).
2. **Hot Context (Active & Timely)**:
   - Current active task, recent architectural decisions, unmerged diffs, and blockers.
3. **Cold Context (Completed / Roadmap)**:
   - Summarise past completed tasks into concise bullet points.
4. **Latest Handoff Section (At the very bottom)**:
   - Summary of changes made in the latest iteration
   - Verification status (`check.sh` result)
   - Next immediate action
5. **Staggered Cleanup Lifecycle**:
   - Do NOT clear context at the same time as AGY (Gemini). Ensure the latest state is captured in `status.md` before executing `/clear`.
