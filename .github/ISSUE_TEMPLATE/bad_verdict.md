---
name: 🎯 Wrong verdict (false positive / false negative)
about: backcheck flagged honest work, or missed something it should have caught
title: '[VERDICT] '
labels: 'false-positive'
assignees: ''

---

<!--
  Please do NOT attach a raw transcript — it contains your code, paths, and possibly secrets.
  A minimal fixture with placeholders is more useful anyway, and can become a regression test.
-->

## What happened

<!-- Which verdict was wrong? e.g. "claimed tests pass → reported unsupported, but pytest did run" -->

## Minimal reproduction

<!-- The smallest JSONL that reproduces it, with paths and content replaced. -->

```jsonl
{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"<command>"}}]}}
{"type":"user","toolUseResult":{"stdout":"<output>","stderr":"","interrupted":false},"message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"<output>"}]}}
{"type":"assistant","message":{"content":[{"type":"text","text":"<what the agent claimed>"}]}}
```

## Expected

<!-- What verdict should backcheck have reached, and why? -->

## Actual

<!-- Paste the output of: backcheck -f your-fixture.jsonl --json -->

```json

```

## Environment

- **backcheck version:** <!-- backcheck --version -->
- **OS:**
- **Test runner / language:** <!-- if relevant -->
