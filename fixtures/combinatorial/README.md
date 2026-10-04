# Combinatorial fixtures

`tier0.pict` is a [PICT](https://github.com/microsoft/pict) model of Tier 0's
input space. Regenerate with:

```sh
pict fixtures/combinatorial/tier0.pict /o:2 \
  | python3 -c 'import csv,json,sys;json.dump(list(csv.DictReader(sys.stdin,delimiter="\t")),sys.stdout,indent=2)' \
  > fixtures/combinatorial/tier0-cases.json
```

The generated cases are **committed** so the test suite does not depend on
`pict` being installed, and so a change in coverage shows up as a reviewable
diff rather than silently varying between machines.

## Why pairwise

The full cross product of the six dimensions is 7,425 combinations. Pairwise
covers every *pair* of parameter values in 56. Most defects in a rules engine
like this come from two features interacting, not six — the one interaction bug
found by hand so far was "next item" silently turning an ordered list unordered,
which is exactly a `Structure` × `Structure` pair.

## Why properties, not golden files

Combinatorial generation gives you inputs; it does not give you an oracle. 56
golden strings would need regenerating whenever a rule changed, and a diff of 56
regenerated strings is unreviewable — which is how a wrong expectation gets
committed.

Instead each generated case is asserted against invariants that must hold
whatever the combination:

- **Tier 0 never invents a word.** It is a filter with no generative step, so
  every word in the output must have been in the input.
- **Carrier words always survive.** Non-filler content is never dropped.
- **A sentence-initial word survives mid-sentence.** Position is the only
  discriminator, so it has to be load-bearing.

## The constraints are a coverage statement

The `IF ... THEN` blocks in the model are not there to make generation
tractable. They record what language data actually exists: structure commands,
spoken punctuation, the stutterable list and multi-word markers are English
only, and cross-language gated fillers exist for `en`, `de` and `fr` alone. The
remaining languages are stubs, so their cases assert pass-through.

When a language table is populated, relax its constraint and regenerate — the
case count going up is the evidence that coverage grew.

## Known limitation: CJK is not whitespace-delimited

Tier 0 tokenises on whitespace. Japanese, Chinese and Cantonese do not delimit
words that way, so those tables cannot be populated under the current design
without a tokeniser. Their cases here assert pass-through, which is correct
today because the tables are empty — but populating them is blocked on more
than vocabulary. Another open-source dictation app documents the same constraint about its
fuzzy matcher.
