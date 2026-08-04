<p align="center">
  <img src="assets/logo.svg" width="70%">
</p>

<p align="center">
  <a href="https://github.com/VectorInstitute/backcheck/actions/workflows/ci.yml">
    <img src="https://github.com/VectorInstitute/backcheck/actions/workflows/ci.yml/badge.svg" alt="CI">
  </a>
  <a href="https://crates.io/crates/backcheck">
    <img src="https://img.shields.io/crates/v/backcheck?color=10B981&label=crates.io" alt="crates.io">
  </a>
  <a href="https://crates.io/crates/backcheck">
    <img src="https://img.shields.io/crates/d/backcheck?color=10B981&label=cargo%20installs" alt="cargo installs">
  </a>
  <a href="https://github.com/VectorInstitute/backcheck/releases">
    <img src="https://img.shields.io/github/downloads/VectorInstitute/backcheck/total?color=0EA5E9&label=binary%20downloads" alt="binary downloads">
  </a>
  <a href="https://github.com/VectorInstitute/backcheck/blob/main/Cargo.toml">
    <img src="https://img.shields.io/crates/msrv/backcheck?color=CE422B" alt="minimum supported Rust version">
  </a>
  <a href="LICENSE">
    <img src="https://img.shields.io/badge/license-Apache--2.0-64748B.svg" alt="Apache 2.0">
  </a>
</p>

---

Your coding agent just said **"All tests pass and I've committed the change."**

Did it run the tests? Did they pass? Did it commit anything?

`backcheck` reads the session transcript Claude Code already writes and checks the agent's
claims against the record of what it actually ran.

```console
$ backcheck

backcheck  session demo1234

  Claims
  ✗ contradicted  lint passes
      “All tests pass, ruff is clean, and I've committed the change.”
      the last `ruff` run 3 failed
      evidence: Found 3 errors.

  ✗ unsupported  changes committed
      “All tests pass, ruff is clean, and I've committed the change.”
      no `git commit` appears in this session

  ~ qualified    tests pass
      “All tests pass, ruff is clean, and I've committed the change.”
      `pytest` passed, but only a subset of tests ran (tests/test_billing.py)
      evidence: 6 passed, 1 skipped in 0.62s

  Test integrity
  ! test disabled  /repo/tests/test_billing.py
      1 skip/ignore marker added to a test file
      @pytest.mark.skip(reason="flaky in CI")

  3 claims not fully supported, 1 sign the test suite was weakened
```

One sentence, three separate things that are not true. The agent hit a failing test and skipped
it rather than fixing it, re-ran only that one file and called it "all tests", reported a linter
as clean when its last run found three errors, and never committed anything at all.

Every one of those is recoverable from the transcript. None of them is visible in the summary
you were meant to read.

## Install

```bash
curl -fsSL https://raw.githubusercontent.com/VectorInstitute/backcheck/main/install.sh | sh
```

Puts a single binary in `~/.local/bin`. No sudo, no runtime dependencies.

With a Rust toolchain, `cargo install backcheck` does the same. Prebuilt binaries for macOS,
Linux, and Windows are on the [releases page](https://github.com/VectorInstitute/backcheck/releases/latest).

## Try it

Nothing to configure. In any project where you have used Claude Code:

```bash
backcheck
```

It reads your most recent session and reports what it could and could not verify. Exit code is
`1` when something does not hold up, so it drops into a pipeline unchanged.

Then wire it into Claude Code so it runs on every session:

```bash
backcheck install            # this project
backcheck install --global   # everywhere
```

As a [Stop hook](https://code.claude.com/docs/en/hooks), `backcheck` inspects the transcript the
moment Claude finishes. When a claim is not supported it blocks completion and hands back the
reasons, so the agent has to run what it said it ran or correct its summary before the turn
ends. Use `--no-block` to report without blocking, and `backcheck uninstall` to remove it.

```bash
backcheck --explain  # show what it recognised, and what ran that it did not
backcheck --json     # machine-readable
backcheck --verbose  # show supported claims too
backcheck --live     # also consult the working tree and git
backcheck --help
```

`--explain` is the one to reach for when a verdict surprises you. The usual reason a claim reads
as unsupported is not that nothing ran, but that the tool which ran is one `backcheck` does not
know yet, and it will say so:

```console
  What backcheck saw
    passed            pytest (test)
        40 passed in 1.30s
    unreadable        eslint (lint)

  Ran, but not recognised as a check
    If one of these is a real check, backcheck is missing a runner for it.
    npm run lint:styles
```

## What it checks

| Claim | Verified against |
|---|---|
| tests pass | test-runner output recorded in the session |
| type check, lint, or build passes | the corresponding tool's output |
| changes committed or pushed | `git` invocations and what they printed |
| file created or written | `Write`/`Edit` calls, and the filesystem with `--live` |

Verdicts are `supported`, `inconclusive`, and the three worth your attention:

- **contradicted**: the run the claim refers to actually failed.
- **unsupported**: nothing in the session backs the claim.
- **qualified**: a real pass that does not mean what the claim implies. Only a subset of tests
  ran, the run stopped at the first failure, a `|| true` swallowed the exit code, or source
  files changed after the last green run and were never re-tested.

It separately flags a suite made to pass rather than made to work: skip and ignore markers
added, assertions weakened (`assertEqual` to `assertTrue`, `toEqual` to `toBeTruthy`),
assertions or test cases deleted, test files removed with `rm`.

## More of what it catches

**The stale green.** The most common one, and the easiest to miss: the suite really did pass,
just not on the code you are about to merge.

```console
  ~ qualified    tests pass
      “Refactored the eviction path. All 214 tests pass.”
      `cargo test` passed, but 2 files changed afterwards and were never re-tested
      (src/cache.rs, src/router.rs)
      evidence: test result: ok. 214 passed; 0 failed; 0 measured; 0 filtered out
```

**The narrow run reported as the whole suite.** Two tests out of 1586, described as "both tests
pass again":

```console
  ~ qualified    tests pass
      `cargo test` passed, but only a subset of tests ran (1584 tests filtered out)
      evidence: test result: ok. 2 passed; 0 failed; 1584 filtered out
```

**The claim with nothing behind it.** No `git commit` ran anywhere in the session:

```console
  ✗ unsupported  changes committed
      “Fixed the proration bug and committed the change.”
      no `git commit` appears in this session
```

Equally important is what it stays quiet about. A passing suite followed by a README edit, a
`pytest -x` run where nothing failed, `ruff --fix` reporting `1 fixed, 0 remaining`, a test
renamed rather than deleted: all fine, all silent. Each of those was a false alarm once, and
each is now a regression test.

## Runners it understands

Verification only works for commands `backcheck` can read the result of, so the list matters.

| | |
|---|---|
| **Tests** | pytest · unittest · tox · nox · cargo test · cargo nextest · go test · jest · vitest · mocha · ava · bun test · npm / yarn / pnpm test · rspec · phpunit · dotnet test · maven · gradle · ctest · make test |
| **CI** | `gh pr checks` · `gh run list` · `gh run view` · `gh run watch` |
| **Types** | mypy · pyright · tsc · cargo check |
| **Lint** | ruff · eslint · clippy · flake8 · pylint · golangci-lint · biome · pre-commit · import-linter · npm lint · cargo fmt · gofmt |
| **Build** | cargo build · go build · npm / vite / Next build · `python -m build` · docker build · mkdocs · sphinx |

It sees through the wrappers these arrive in: `uv run`, `poetry run`, `npx`, `pnpm exec`,
`python -m`, `timeout 240 …`, shell loops, and virtualenv paths like `.venv/bin/python -m pytest`.

Missing yours? [Adding one](CONTRIBUTING.md#adding-a-runner) is three steps in a single file and
the most useful first contribution to the project ([#3](https://github.com/VectorInstitute/backcheck/issues/3)).

## How it works

Claude Code writes every session to `~/.claude/projects/<project>/<session>.jsonl`. `backcheck`
reads that file twice, separately.

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

The two halves never inform each other. That is the whole trick: the agent's account of a
session cannot influence the record of it.

One wrinkle drives much of the design. **Transcripts do not record exit codes.** Whether a
command succeeded has to be recovered from what it printed, so `backcheck` carries a parser per
runner (pytest's `1 failed, 47 passed`, cargo's `test result: FAILED`, jest's `Tests: 1 failed`)
and returns *inconclusive* rather than guessing. Ordering matters too: a pass counts only if it
happened before the claim and after the last source edit.

No model is called, so runs are deterministic, free, and offline: 213 MB of real transcripts in
1.3 s, a typical session in under 30 ms.

Those transcripts are also how it was tested, in two ways. Across 80 real Claude Code sessions
(219 MB) its "was a suite actually run" conclusion was cross-checked against an independent scan
of the raw JSONL: 36 of 36 in agreement. Separately, a dozen sessions were read line by line and
adjudicated by hand first, then compared against what `backcheck` reported. That second pass is
where the interesting bugs were, and all of them are now regression tests. A sample:

- a chained `pytest …; echo ---; ruff …` fed both parsers the whole stream, so a linter's "All
  checks passed!" was cited as the evidence that tests passed
- `cargo test <name>` passing 2 of 1586 tests was reported as a clean pass, not a subset
- `Found 1 error (1 fixed, 0 remaining)` from `ruff --fix` was read as a failure
- a formatter wrapping an assertion across lines looked like the comparison had been removed
- `pytest tests/test_x.py tests/` runs the whole suite, but the filename made it look narrow
- a shell error in a later part of a chain marked an already-finished `pytest` as interrupted
- a CI failure at the start of a session contradicted a claim made thirty steps later, after
  the problem had been fixed
- "Test of Time → green gradient banner" was read as a passing suite, and "pushed the last item
  over" as a `git push`

## Limitations

- Claims are found by pattern matching over prose. Unusual phrasing is missed. `backcheck` is
  tuned to stay quiet rather than flag everything, because a hook that cries wolf gets
  uninstalled.
- A summary referring to work from an earlier session reads as unsupported, since the evidence
  is in a different transcript.
- Test-integrity findings are signals, not verdicts. Skipping a genuinely broken test is
  legitimate; `backcheck` shows you the edit and you decide.
- Only Claude Code transcripts are read today. The parser is isolated in
  [`src/transcript.rs`](src/transcript.rs), so other agents are a contained change
  ([#5](https://github.com/VectorInstitute/backcheck/issues/5)).

## Contributing

Good places to start:

- [#3](https://github.com/VectorInstitute/backcheck/issues/3): **teach it a runner it doesn't
  know.** Three steps in one file, and each one makes the tool correct for a whole ecosystem.
- [#9](https://github.com/VectorInstitute/backcheck/issues/9): **verify a new kind of claim**
  ("I ran the migration", "I removed the debug logging").
- [#5](https://github.com/VectorInstitute/backcheck/issues/5): **support another agent.**
- [#10](https://github.com/VectorInstitute/backcheck/issues/10): **more install routes**
  (Homebrew, `npx`).

Found a transcript where `backcheck` got it wrong? That is the most valuable bug report there
is, and [#6](https://github.com/VectorInstitute/backcheck/issues/6) explains how to send one
with the sensitive parts removed. See [CONTRIBUTING.md](CONTRIBUTING.md) for the design rules.

## License

Apache 2.0. See [LICENSE](LICENSE).

Built at the [Vector Institute](https://vectorinstitute.ai).
