# Contributing to backcheck

Thanks for looking. The most useful contributions here are small and concrete: a runner that
isn't recognised, a claim phrasing that slips through, a false positive that made you stop
trusting the tool.

## Getting set up

Rust 1.85 or newer is all you need.

```bash
git clone https://github.com/VectorInstitute/backcheck
cd backcheck
cargo test
cargo run -- --help
```

Before opening a pull request:

```bash
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

## Reporting a bad verdict

A transcript where `backcheck` got it wrong is the most valuable bug report there is, and also
the most awkward to send — transcripts contain your code, your paths, and sometimes your
secrets. **Never attach a raw transcript.**

Send instead the smallest set of lines that reproduces the problem, with paths and content
replaced by placeholders. A JSONL fixture of five lines is usually enough, and it can go
straight into `tests/fixtures/` as a regression test:

```jsonl
{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"<the command>"}}]}}
{"type":"user","toolUseResult":{"stdout":"<the output>","stderr":"","interrupted":false},"message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"<the output>"}]}}
{"type":"assistant","message":{"content":[{"type":"text","text":"<what the agent claimed>"}]}}
```

Tell us what you expected and what you got. `backcheck -f your-fixture.jsonl --json` output is
a good thing to paste.

## Adding a runner

This is the best first contribution: self-contained, obviously useful, and the same three steps
every time. All of it lives in [`src/runners.rs`](src/runners.rs).

1. **Recognise the command** in `classify()`. Return the `CheckKind` and a short runner name.
2. **Read its outcome** in `parse_outcome()`. Match on your runner name and pull the verdict out
   of its summary line. Return `Outcome::Unknown` when the output doesn't say — a wrong
   "verified" is far more damaging than an honest "could not tell".
3. **Test both directions.** One passing output, one failing output, copied from a real run:

```rust
#[test]
fn my_runner_summary() {
    let ok = one("myrunner", "<real passing output>");
    assert_eq!(ok.outcome, Outcome::Passed);
    let bad = one("myrunner", "<real failing output>");
    assert_eq!(bad.outcome, Outcome::Failed);
}
```

If the runner has flags that qualify a pass — running a subset, stopping at the first failure —
add them to `caveats_for()` too.

## Design rules

These are what keep the tool trustworthy. Please hold to them.

**No model calls, ever.** Every verdict must be derived from recorded output. Determinism is the
product: the same transcript must always produce the same result, at no cost, offline.

**Silence is the default.** A hook that fires on honest work gets uninstalled within a day, and
then it protects nobody. When a signal is ambiguous, return `Inconclusive` and say nothing.
False positives are far more expensive here than false negatives.

**Claims and evidence stay separate.** Nothing in the evidence path may read the agent's prose,
and nothing in the claim path may read tool results. That independence is the reason the output
means anything.

**Every verdict shows its evidence.** A verdict the user has to take on faith is just another
unverified claim.

**Never break a session.** Hook mode must degrade to "say nothing and let the turn finish" on
any malformed input, missing file, or internal error.

## Pull requests

Keep them focused — one runner, one fix, one feature. Include a test that fails without your
change. If you're planning something larger, open an issue first so we can agree on the shape
before you write it.

## Code of conduct

This project follows the [Contributor Covenant](CODE_OF_CONDUCT.md).
