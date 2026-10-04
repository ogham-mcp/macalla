#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
"""Word error rate for each engine against a known reference.

One impression was that Canary handles technical terms well. Impressions
are worth testing: this scores all three on identical audio with a reference
transcript, so the comparison is arithmetic rather than a read-through.

WER is the standard metric -- substitutions plus deletions plus insertions,
over reference length. Reported alongside a deletion count on its own, because
a model that silently drops a clause is a different failure from one that
mishears a word, and WER alone hides that.
"""
import json, re, sys

def norm(t):
    t = t.lower().replace("’", "'")
    return [w for w in re.split(r"[^a-z0-9']+", t) if w]

def align(ref, hyp):
    """Levenshtein over words, returning (sub, del, ins)."""
    n, m = len(ref), len(hyp)
    d = [[0] * (m + 1) for _ in range(n + 1)]
    op = [[None] * (m + 1) for _ in range(n + 1)]
    for i in range(n + 1):
        d[i][0] = i; op[i][0] = "d"
    for j in range(m + 1):
        d[0][j] = j; op[0][j] = "i"
    op[0][0] = None
    for i in range(1, n + 1):
        for j in range(1, m + 1):
            cands = [
                (d[i - 1][j - 1] + (ref[i - 1] != hyp[j - 1]),
                 "c" if ref[i - 1] == hyp[j - 1] else "s"),
                (d[i - 1][j] + 1, "d"),
                (d[i][j - 1] + 1, "i"),
            ]
            d[i][j], op[i][j] = min(cands)
    i, j = n, m
    counts = {"s": 0, "d": 0, "i": 0, "c": 0}
    while i > 0 or j > 0:
        o = op[i][j]
        counts[o] += 1
        if o in ("c", "s"): i, j = i - 1, j - 1
        elif o == "d": i -= 1
        else: j -= 1
    return counts

def score(ref_text, hyp_text):
    ref, hyp = norm(ref_text), norm(hyp_text)
    c = align(ref, hyp)
    wer = (c["s"] + c["d"] + c["i"]) / len(ref) if ref else 0.0
    return wer, c, len(ref)

def main():
    data = json.load(open(sys.argv[1]))
    print(f"{'fixture':<18}{'engine':<12}{'WER':>7}{'sub':>6}{'del':>6}{'ins':>6}{'ref':>6}")
    for fixture, block in data.items():
        ref = block["reference"]
        for engine, hyp in block["hyps"].items():
            wer, c, n = score(ref, hyp)
            flag = "  <- drops content" if c["d"] > n * 0.10 else ""
            print(f"{fixture:<18}{engine:<12}{wer:>7.3f}{c['s']:>6}{c['d']:>6}{c['i']:>6}{n:>6}{flag}")
        print()

main()
