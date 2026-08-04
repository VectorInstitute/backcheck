# Submission draft: Awesome Claude Code

Not yet eligible. The list requires **14 days of development on the default branch, or 100
stars**. First commit to `main` was 2026-08-03, so the earliest submission date is
**2026-08-17**.

The maintainer requires the submission to be made **by a human, through the web issue form**.
Opening it any other way risks being restricted from the repository:

> ALL RECOMMENDATIONS MUST BE MADE USING THE WEB UI ISSUE FORM TEMPLATE, OR YOU RISK BEING
> RESTRICTED FROM INTERACTING WITH THIS REPOSITORY TEMPORARILY.

> Although resources themselves may be partially or entirely written by a coding agent,
> resource recommendations must be created by human beings.

Form: <https://github.com/hesreallyhim/awesome-claude-code/issues/new?template=recommend-resource.yml>

---

## Field values

**Display Name**

```
backcheck
```

**Category**

```
Linting
```

Their `Observability & Monitoring` section is live dashboards and session monitors, which this
is not. `Linting` holds tools that validate something and report violations, which is what this
does to an agent's closing summary. The maintainer may recategorise; that is fine.

**Link**

```
https://github.com/VectorInstitute/backcheck
```

**Author Name**

```
VectorInstitute
```

**Author Link**

```
https://github.com/VectorInstitute
```

**Description** (their style rules: a description not a pitch, no addressing the reader, one
line, no emojis, 10 to 500 characters)

```
Reads a Claude Code session transcript and checks the agent's closing claims, such as tests passing or changes being committed, against the tool calls that actually ran. Each claim is reported as supported, qualified, unsupported, or contradicted, together with the line of recorded output the verdict rests on. Runs as a Stop hook, a CLI, or in CI, and makes no model calls.
```

**Checklist**: tick the first five. Leave the sixth unchecked; it is a trap for people who do not
read the form.

---

## Before submitting

- Confirm the repo still has a detectable licence. GitHub currently reports `Apache-2.0`.
- Re-read the entry against the list's existing `Linting` entries so the description matches
  their register.
- Check the resource is not already listed, and that no near-duplicate was added in the interim.

## Worth knowing first

The maintainer is explicit that the list is not a growth channel:

> Too many people think like this: (i) Build something awesome; (ii) Submit to Awesome Claude
> Code; (iii) Get accepted, because of being awesome; (iv) Get users. However, a more likely
> chain of events is: (i) Build something awesome; (ii) Get users; (iii) Submit it to Awesome
> Claude Code.

If approved, the list invites a badge:

```markdown
[![Mentioned in Awesome Claude Code](https://awesome.re/mentioned-badge.svg)](https://github.com/hesreallyhim/awesome-claude-code)
```
