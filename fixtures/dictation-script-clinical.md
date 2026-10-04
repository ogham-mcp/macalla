# Dictation script — clinical

A second domain, to test whether the project's central claim holds: that a clinician's or a
lawyer's vocabulary fails the same way an engineer's does, and is fixed the same
way.

If it does not hold, the claim overclaims and needs narrowing.

Read aloud for `./bench/record-and-compare.sh`, same instructions as
`dictation-script.md` — say the bracketed sounds, do not tidy as you go.

---

[um] The patient was started on amoxicillin, [uh], five hundred milligrams
three times daily.

We switched from atorvastatin to rosuvastatin because of myalgia, [er], and
added amlodipine for the hypertension.

[hmm] She reports dyspnoea on exertion and intermittent palpitations,
[you know], with no syncope.

The echocardiogram showed a preserved ejection fraction, [so] we ruled out
cardiomyopathy.

Continue levothyroxine seventy five micrograms, omeprazole twenty milligrams,
and salbutamol as required.

[um] Refer to cardiology, and repeat the full blood count and the urea and
electrolytes in two weeks.

---

## What this tests

| Line | Terms |
|---|---|
| 1 | `amoxicillin`, dosing read as words |
| 2 | `atorvastatin`, `rosuvastatin`, `myalgia`, `amlodipine` |
| 3 | `dyspnoea`, `palpitations`, `syncope` |
| 4 | `echocardiogram`, `ejection fraction`, `cardiomyopathy` |
| 5 | `levothyroxine`, `omeprazole`, `salbutamol`, `micrograms` |
| 6 | `cardiology`, `urea and electrolytes` |

British spellings throughout — `dyspnoea`, not `dyspnea` — because an engine
Americanising a clinician's notes is the same class of error as one renaming
`postgres` to `PostgreSQL`.
