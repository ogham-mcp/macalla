# SPDX-License-Identifier: MIT
"""Scores a bench/enhance_ab.sh or bench/enhance_endpoint.sh run.

    python3 bench/enhance_ab_score.py <out-dir>

One line per run. p50 is the median; p95 is nearest rank, so over 20 requests
it is the 19th fastest, and the slowest is shown beside it. "Unchanged" counts
pieces that came back as their Tier 0 text. "Rejected" is how many of those the
guard threw out, counted in the run's --debug log; the rest were returned as
sent. Replies are compared row by row between two runs of one model, and
between the builds of an A/B.

Then each run's final text, which is the reply or the Tier 0 text the guard
kept, is scored against the reference punctuation in bench/fixtures.json:
"exact" pieces have every capital and mark right; "case" counts words whose
capitals differ; "missing", "extra" and "wrong" count the gaps after words
where a mark was left out, added where none belongs, or is the wrong one. A
piece whose words differ from the reference is "unscored". Tier 0's own text is
scored the same way, as the baseline a provider has to beat.
"""

import json
import math
import re
import statistics
import sys
from pathlib import Path

AB_ORDER = ["a1", "b1", "b2", "a2"]
NOT_RUNS = {"prewarm", "first"}
REJECTED = "Tier 1 reply rejected"
FIXTURES = Path(__file__).with_name("fixtures.json")
# The hyphens and dashes the Tier 1 guard treats as a space; keep the two lists
# the same. Hyphen-minus first, so it is literal inside a character class.
DASHES = "-‐‑–—"


def rows(path):
    if not path.exists():
        return []
    return [json.loads(line) for line in path.read_text().splitlines() if line.strip()]


def p95(values):
    ordered = sorted(values)
    return ordered[math.ceil(0.95 * len(ordered)) - 1]


def same_replies(x, y):
    pairs = list(zip(x, y))
    return sum(1 for p, q in pairs if "enhanced" in p and p.get("enhanced") == q.get("enhanced")), len(pairs)


def split_word(token):
    """The word between a token's first and last letter or digit, and what follows it."""
    alnum = [i for i, c in enumerate(token) if c.isalnum()]
    if not alnum:
        return "", token
    return token[alnum[0] : alnum[-1] + 1], token[alnum[-1] + 1 :]


def words_and_marks(text):
    """Each word with the marks after it. A hyphen or dash separates words as
    the guard's does, so "ninety-five" is ninety, a hyphen, then five; a
    free-standing dash joins the word before."""
    out = []
    for token in re.sub(f"([{DASHES}])(?=\\S)", r"\1 ", text).split():
        word, marks = split_word(token)
        if word:
            out.append([word, marks])
        elif out:
            out[-1][1] += marks
    return out


def parse_reference(text):
    """Per word: the spelling to match (None when case is not scored), the word
    lowercased, and the marks accepted after it. See the notation in
    bench/fixtures.json."""
    out = []
    for token in text.split():
        free_case = token.startswith("~")
        word, marks = split_word(token.lstrip("~"))
        if marks.startswith("(") and marks.endswith(")"):
            accepted = {""} | set(marks[1:-1])
        elif marks.startswith("{") and marks.endswith("}"):
            accepted = set(marks[1:-1])
        else:
            accepted = {marks}
        out.append((None if free_case else word, word.lower(), accepted))
    return out


def against_reference(text, reference):
    """(case, missing, extra, wrong) for one piece, or None when its words differ."""
    got = words_and_marks(text)
    if [word.lower() for word, _ in got] != [lower for _, lower, _ in reference]:
        return None
    case = sum(1 for (word, _), (spelling, _, _) in zip(got, reference) if spelling not in (None, word))
    missing = extra = wrong = 0
    for (_, marks), (_, _, accepted) in zip(got, reference):
        if marks in accepted:
            continue
        if not marks:
            missing += 1
        elif "" in accepted:
            extra += 1
        else:
            wrong += 1
    return case, missing, extra, wrong


def references_for(out):
    """The reference for each row, in the order the run sent the fixtures."""
    by_raw = {c["raw"]: parse_reference(c["reference"]) for c in json.loads(FIXTURES.read_text())["cases"]}
    sent = out / "fixtures.txt"
    raws = sent.read_text().splitlines() if sent.exists() else list(by_raw)
    return [by_raw.get(raw) for raw in raws]


def quality(texts, references):
    """Totals over one run's pieces, and which pieces were exact."""
    exact, totals, unscored = [], [0, 0, 0, 0], 0
    for text, reference in zip(texts, references):
        score = against_reference(text, reference) if text is not None and reference else None
        if score is None:
            unscored += 1
            exact.append(False)
            continue
        totals = [t + s for t, s in zip(totals, score)]
        exact.append(not any(score))
    return exact, totals, unscored


def print_quality(runs, references):
    print(f"\n{'against the reference':<22} {'exact':>5} {'case':>5} {'missing':>7} {'extra':>5} {'wrong':>5} {'unscored':>8}")
    lines, per_fixture, baselines = [], [[] for _ in references], {}
    for name, run in runs.items():
        final = [None if "error" in r else r.get("enhanced") for r in run]
        tier0 = tuple(r.get("tier0") for r in run)
        baselines.setdefault(tier0, name)
        lines.append((name, final))
    for tier0, name in baselines.items():
        lines.append((f"Tier 0 (as {name})", list(tier0)))
    for label, texts in lines:
        exact, (case, missing, extra, wrong), unscored = quality(texts, references)
        if not label.startswith("Tier 0"):
            for i, ok in enumerate(exact):
                per_fixture[i].append(ok)
        print(f"{label:<22} {sum(exact):>2}/{len(texts):<2} {case:>5} {missing:>7} {extra:>5} {wrong:>5} {unscored:>8}")
    hard = [f"{i + 1} ({sum(oks)}/{len(oks)})" for i, oks in enumerate(per_fixture) if oks and sum(oks) * 2 < len(oks)]
    if hard:
        print("exact in fewer than half the runs, fixture (runs exact): " + ", ".join(hard))


def main():
    out = Path(sys.argv[1])
    names = [p.stem for p in out.glob("*.jsonl") if p.stem not in NOT_RUNS]
    names.sort(key=lambda n: (AB_ORDER.index(n) if n in AB_ORDER else len(AB_ORDER), n))
    runs = {n: rows(out / f"{n}.jsonl") for n in names}

    print(f"{'run':<18} {'rows':>4} {'errors':>6} {'p50':>6} {'p95':>6} {'slowest':>7} {'unchanged':>9} {'rejected':>8}")
    for name, run in runs.items():
        errors = sum(1 for r in run if "error" in r)
        times = [r["ms"] for r in run if "error" not in r]
        unchanged = sum(1 for r in run if r.get("unchanged"))
        log = out / f"{name}.err"
        rejected = str(log.read_text().count(REJECTED)) if log.exists() else "-"
        p50 = f"{statistics.median(times):.0f}" if times else "-"
        tail = f"{p95(times)}" if times else "-"
        slowest = f"{max(times)}" if times else "-"
        mark = "  <- has errors; later runs are suspect" if errors else ""
        print(f"{name:<18} {len(run):>4} {errors:>6} {p50:>6} {tail:>6} {slowest:>7} {unchanged:>9} {rejected:>8}{mark}")

    pairs = []
    if all(n in runs for n in AB_ORDER):
        pairs += [("A against B", "a1", "b1"), ("A twice", "a1", "a2"), ("B twice", "b1", "b2")]
    for n in names:
        if n.endswith("-1") and f"{n[:-2]}-2" in runs:
            pairs.append((f"{n[:-2]} twice", n, f"{n[:-2]}-2"))
    for label, x, y in pairs:
        same, total = same_replies(runs[x], runs[y])
        print(f"same replies, {label}: {same}/{total}")

    if runs:
        print_quality(runs, references_for(out))

    for kind in ("first", "prewarm"):
        records = rows(out / f"{kind}.jsonl")
        if records:
            print()
        for record in records:
            row = record["row"] or {"error": "no output"}
            detail = row.get("error") or row.get("enhanced")
            print(f"{record['label']}: {row.get('ms', '-')} ms  {detail}")


if __name__ == "__main__":
    main()
