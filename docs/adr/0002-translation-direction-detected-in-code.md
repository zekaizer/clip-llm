# ADR-0002: Decide the translation direction in code with a script heuristic

- Status: accepted
- Date: 2026-09-06

## Context

The Translate prompt asked the model to pick the direction itself ("if the
input is mostly Korean, translate to English; in every other case, to
Korean"). That judgement fails on exactly the inputs this tool sees most:
grok-4.3 returns code with Korean comments unchanged (harness F-001 and
T-012, 4/4 runs) because the code makes the input "not mostly Korean", and
gemma-4-e2b with thinking off returns English release notes unchanged (3/3).
Stating the direction in the prompt fixed both (grok 4/4, gemma 3/3), so
the direction has to be decided before the request is built.

Two ways to decide it were measured on 47 samples (the translate vectors
plus code with Korean comments, Korean prose dense with English terms,
English with Korean clauses, logs, tables, other languages, single words):

| Method | Correct | Cost |
|---|---|---|
| prose heuristic: grammar markers, then Hangul/Latin ratio | 45 | none |
| `whichlang` 0.1 | 42 | 812 KB source, embedded weights |
| `whatlang` 0.16 | 36 | 756 KB source |
| `lingua` 1.8 (Korean + English models) | 36 | 28 MB source + 4.8 MB models, slow build |

The language-identification crates answer "which language is this text"
and are trained on monolingual text. Korean prose full of English terms
and code with Korean comments come out as English, which is the failure
being fixed. The question here is narrower: is the *prose* Korean, given
that Hangul is unambiguous and everything non-Korean targets Korean anyway.

## Decision

`src/lang.rs` decides both the direction and dictionary lookups with a
dependency-free heuristic:

- Tokens with symbol characters (identifiers, paths, URLs, operators),
  ALL-CAPS tokens (keywords, acronyms) and statement lines (ending in `;`,
  `{`, `}`) are not prose; Hangul is counted everywhere.
- Korean particles and sentence endings against English function words
  decide first: they say which language a sentence is written in even when
  English terms outnumber the Korean words.
- On a tie, the script ratio decides, one Hangul syllable weighed as 2.5
  Latin letters; no prose at all is "not Korean".
- A lookup is a single plain word with no sentence punctuation.

The Translate prompt states the decided direction instead of the rule; the
rule text remains only as the fallback when there is no text (image
input) or the primary language is not Korean. The 117-case fixture
`scripts/lang_direction_vectors.json` is shared by the Rust unit tests and
the harness self-tests, which mirror the heuristic in Python.

## Consequences

- Deterministic and free: no model call, no dependency, no binary growth,
  and the decision is unit-tested against a fixture instead of sampled.
- Prefix-cache friendly: the direction is a function of the content, so
  the same input always produces the same system prompt.
- Korean-specific: the detector recognises Hangul only. A config with a
  different primary language keeps the model-side rule.
- Known limits, recorded in the fixture: romanized Korean counts as
  non-Korean (the rule sends it to Korean), and a line that is half English
  sentence and half Korean sentence is labelled "either".
- The harness must keep its Python mirror in step; the shared fixture
  makes a drift fail both test suites.
