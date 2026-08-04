<p align="center">
  <img src="assets/logo.svg" width="70%">
</p>

<p align="center">
  <a href="https://github.com/VectorInstitute/backcheck/actions/workflows/ci.yml">
    <img src="https://github.com/VectorInstitute/backcheck/actions/workflows/ci.yml/badge.svg" alt="CI">
  </a>
  <a href="https://crates.io/crates/backcheck">
    <img src="https://img.shields.io/crates/v/backcheck.svg" alt="crates.io">
  </a>
  <img src="https://img.shields.io/badge/rust-≥1.82-CE422B.svg" alt="Rust ≥ 1.82">
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-Apache--2.0-blue.svg" alt="Apache 2.0"></a>
  <img src="https://img.shields.io/badge/LLM_calls-0-48C0D9.svg" alt="Zero LLM calls">
</p>

---

Your coding agent just said **"All tests pass and I've committed the change."**

Did it run the tests? Did they pass? Did it commit anything?

`backcheck` reads the session transcript Claude Code already writes and holds the agent's
claims against the record of what it actually ran. It is a linter for the summary at the end
of a session.

```console
$ backcheck

backcheck  session demo1234
  tests/fixtures/demo.jsonl

  Claims
  ✗ unsupported  changes committed
      “All tests pass and I've committed the change.”
      no `git commit` appears in this session

  ~ qualified    tests pass
      “All tests pass and I've committed the change.”
      `pytest` passed, but only a subset of tests ran (tests/test_billing.py)
      evidence: 6 passed, 1 skipped in 0.62s

  Test integrity
  ! test disabled  /repo/tests/test_billing.py
      1 skip/ignore marker added to a test file
      @pytest.mark.skip(reason="flaky in CI")

  2 claims not fully supported, 1 sign the test suite was weakened
```

That session really happened: the agent hit a failing test, skipped it, re-ran only the one
file, and reported total success. Every part of that is recoverable from the transcript, and
none of it is visible in the summary you were meant to read.

## Why this exists

Agents write more code than anyone can read, and the summary at the end is the compression
everyone relies on. When that summary is wrong the failure is silent — a green ✅ that nothing
backs. The [documented](https://dev.to/moonrunnerkc/ai-agents-cheat-on-pull-requests-i-mined-327-of-them-to-prove-it-43ij)
failure modes are mundane rather than dramatic: tests that were never run, a suite that passed
before the last three edits, an assertion quietly softened to make a red bar green.

Existing tooling asks the agent to check its own work. `backcheck` does not ask the agent
anything. It reads the evidence.

## Install

```bash
cargo install backcheck
```

Or build from source:

```bash
git clone https://github.com/VectorInstitute/backcheck
cd backcheck && cargo install --path .
```

Prebuilt binaries for macOS and Linux are attached to every
[release](https://github.com/VectorInstitute/backcheck/releases).

## Use

Check the most recent session in the current project:

```bash
backcheck
```

Wire it into Claude Code so it runs automatically when a session ends:

```bash
backcheck install          # this project
backcheck install --global # every project
```

Installed as a [Stop hook](https://code.claude.com/docs/en/hooks), `backcheck` inspects the
transcript the moment Claude finishes. If claims are unsupported it blocks completion and hands
the reasons back, so the agent has to either run what it said it ran or correct its summary
before the turn ends. `backcheck install --no-block` reports without blocking.

Other things it does:

```bash
backcheck --json                    # machine-readable, for CI
backcheck --verbose                 # show supported claims too
backcheck --live                    # also consult the working tree and git
backcheck -f path/to/session.jsonl  # a specific transcript
backcheck uninstall
```

Exit code is `1` when something is unsupported, so it drops into a pipeline unchanged.

## What it checks

**Claims**, extracted from what the agent wrote in prose:

| Claim | Verified against |
|---|---|
| tests pass | test-runner output recorded in the session |
| type check / lint / build passes | the corresponding tool's output |
| changes committed / pushed | `git` invocations and what they printed |
| file created or written | `Write`/`Edit` calls, and the filesystem with `--live` |

Each claim gets one of five verdicts. `supported` and `inconclusive` are quiet; the other three
are what you came for:

- **contradicted** — the run the claim refers to actually failed.
- **unsupported** — nothing in the session backs the claim at all.
- **qualified** — a real pass, but one that does not mean what the claim implies: only a subset
  of tests ran, the run stopped at the first failure, a `|| true` swallowed the exit code, or
  source files changed after the last green run and were never re-tested.

**Test integrity**, from the edits themselves — a suite made to pass rather than made to work:

- skip and ignore markers added (`@pytest.mark.skip`, `.skip(`, `#[ignore]`, `t.Skip`, `@Disabled`, …)
- assertions weakened (`assertEqual` → `assertTrue`, `toEqual` → `toBeTruthy`, `assert x == y` → `assert x`)
- assertions or whole test cases deleted
- test files removed with `rm`

Runners understood today: pytest, unittest, tox, nox, cargo test, cargo nextest, go test, jest,
vitest, mocha, ava, bun test, npm/yarn/pnpm test, rspec, phpunit, dotnet test, maven, gradle,
ctest, make test — plus mypy, pyright, tsc, ruff, eslint, clippy, flake8, pylint, golangci-lint,
biome. [Adding one](CONTRIBUTING.md#adding-a-runner) is a small, self-contained change.

## How it works

Claude Code writes every session to `~/.claude/projects/<project>/<session>.jsonl`: assistant
messages, tool calls, and tool results, in order. `backcheck` reads that file twice, separately.

```
session.jsonl
   │
   ├─ prose ─────────► claims      "all tests pass", "committed"
   │                       │
   └─ tool calls ────► evidence    pytest → "1 failed, 47 passed"
                           │       Edit  → tests/test_billing.py
                           ▼
                       verdict per claim + test-integrity findings
```

The two halves never inform each other, which is the entire trick: the agent's account of the
session cannot influence the record of it.

One wrinkle drives much of the design. **Transcripts do not record exit codes.** Whether a
command succeeded has to be recovered from what it printed, so `backcheck` carries a parser per
runner — pytest's `1 failed, 47 passed`, cargo's `test result: FAILED`, jest's `Tests: 1 failed`
— and returns *inconclusive* rather than guessing when it cannot tell. Ordering matters too: a
pass only counts if it happened before the claim and after the last source edit.

No model is called. Every verdict comes from recorded output, so runs are deterministic, free,
and fast enough to be invisible: **213 MB of real transcripts analysed in 1.3 s**, a typical
session in under 30 ms.

Those transcripts are also how it was tested. `backcheck` was run against 78 real Claude Code
sessions, and its "was a test suite actually run" conclusion was cross-checked against an
independent scan of the raw JSONL — 36 of 36 in agreement. Two bugs found that way are now
regression tests: runners invoked through a virtualenv path (`.venv/bin/python -m pytest`) were
invisible, and the shell builtin `test -f` was being counted as a test run.

## Limitations

Worth knowing before you trust it:

- Claims are found with pattern matching over prose. Unusual phrasing gets missed; `backcheck`
  is tuned to stay quiet rather than to flag everything, because a hook that cries wolf gets
  uninstalled.
- A summary that refers to work from an *earlier* session reads as unsupported, because the
  evidence is in a different transcript.
- Test-integrity findings are signals, not verdicts. Skipping a genuinely broken test is a
  legitimate move; `backcheck` shows you the edit and you decide.
- Only Claude Code transcripts are read today. The parser is isolated in
  [`src/transcript.rs`](src/transcript.rs) — other agents are a contained change, and
  [#5](https://github.com/VectorInstitute/backcheck/issues/5) is open for it.

## Contributing

Issues labelled [`good first issue`](https://github.com/VectorInstitute/backcheck/issues?q=is%3Aissue+is%3Aopen+label%3A%22good+first+issue%22)
are scoped to be a single self-contained change with an obvious test. Good places to start:

- [#3](https://github.com/VectorInstitute/backcheck/issues/3) — **teach it a test runner it doesn't know.**
  Three steps in one file, and every runner added makes the tool correct for a whole ecosystem.
- [#9](https://github.com/VectorInstitute/backcheck/issues/9) — **verify a new kind of claim**
  ("I ran the migration", "I removed the debug logging").
- [#5](https://github.com/VectorInstitute/backcheck/issues/5) — **support another agent.**
  Only the transcript parser is Claude-specific; everything downstream is already agnostic.
- [#10](https://github.com/VectorInstitute/backcheck/issues/10) — **make it installable without
  a Rust toolchain** (Homebrew, `npx`, `curl | sh`).

Have a transcript where `backcheck` got it wrong? That is the most valuable bug report there is
— [#6](https://github.com/VectorInstitute/backcheck/issues/6) explains how to send one with the
sensitive parts removed. Two of the sharpest bugs found so far came in exactly that way.

## License

Apache 2.0 — see [LICENSE](LICENSE).

Built at the [Vector Institute](https://vectorinstitute.ai).
