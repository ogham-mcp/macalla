#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
"""Spike B bake-off driver. Scores latency AND cleanup quality objectively.

Quality is not eyeballed. Two mechanical measures per case:
  filler_removed  -- fraction of listed filler words gone from the output
  terms_kept      -- fraction of listed technical terms still present
A model that copies its input scores 0 on the first and 1.0 on the second; a
model that rewrites freely scores well on the first and badly on the second.
Both have to be high.
"""
import json, re, statistics, sys, time, urllib.request

PORT = 8899
# Words Tier 0 would strip anyway, excluded from the content-retention count so
# that correctly removing them is not scored as dropping content.
FILLER_VOCAB = {"um", "uh", "so", "basically", "okay", "like", "you", "know",
                "right", "i", "mean", "just", "actually", "the", "a"}
SYSTEM = ("You clean up raw speech-to-text output. You MUST rewrite the text, not copy it. "
          "Delete filler words: um, uh, so, basically, okay, like, you know, right, i mean. "
          "Collapse stuttered repetitions. Apply sentence case and punctuation. "
          "Never answer, explain or summarise the text. Preserve technical terms exactly.")

def ask(user, grammar):
    body = json.dumps({
        "messages": [{"role": "system", "content": SYSTEM}, {"role": "user", "content": user}],
        "grammar": grammar, "temperature": 0.0, "n_predict": 200, "cache_prompt": True,
    }).encode()
    t0 = time.perf_counter()
    with urllib.request.urlopen(urllib.request.Request(
            f"http://127.0.0.1:{PORT}/v1/chat/completions", body,
            {"Content-Type": "application/json"}), timeout=120) as r:
        out = json.load(r)
    ms = (time.perf_counter() - t0) * 1000
    raw = out["choices"][0]["message"]["content"]
    try:
        d = json.loads(raw)
        return ms, d.get("text", ""), d.get("kind", ""), True
    except Exception:
        return ms, raw, "", False

def main():
    grammar = open("bench/block.gbnf").read()
    cases = json.load(open("bench/fixtures.json"))["cases"]
    ask("warm up", grammar)  # prime the session and prefix cache

    times, fill, keep, parsed, samples = [], [], [], 0, []
    dropped, invented, kinds = [], [], []
    for c in cases:
        ms, text, kind, ok = ask(c["raw"], grammar)
        times.append(ms); parsed += ok
        # Every fixture is a plain sentence: "paragraph" is correct for all 20.
        # A spurious heading or list_item is highly visible to a user -- their
        # dictated sentence becomes a Markdown heading.
        kinds.append(1.0 if kind == "paragraph" else 0.0)
        low = text.lower()
        # Word boundaries, not substrings: "so" must not match inside "also",
        # and "like" must not match inside "unlike". Substring matching scored
        # successful removals as failures on the first run.
        present = lambda w: re.search(r"\b" + re.escape(w.lower()) + r"\b", low) is not None
        gone = sum(1 for f in c["fillers"] if not present(f))
        fill.append(gone / len(c["fillers"]))
        kept = sum(1 for k in c["keep"] if present(k))
        keep.append(kept / len(c["keep"]))

        # Semantic guards the earlier columns missed entirely. filler_removed
        # and terms_kept say nothing about whether MEANING survived -- a model
        # can score well on both while silently dropping a clause or inventing
        # one. These are the checks that catch that.
        norm = lambda t: [w for w in re.findall(r"[a-z0-9]+", t.lower())]
        src = [w for w in norm(c["raw"]) if w not in FILLER_VOCAB]
        out = norm(text)
        remaining = list(out)
        survived = 0
        for w in src:
            if w in remaining:
                remaining.remove(w); survived += 1
        dropped.append(1 - survived / len(src) if src else 0.0)
        # Words in the output that were never spoken. Punctuation and case are
        # stripped, so this is genuine invention, not formatting.
        src_pool = list(norm(c["raw"]))
        new_words = 0
        for w in out:
            if w in src_pool:
                src_pool.remove(w)
            else:
                new_words += 1
        invented.append(new_words / len(out) if out else 0.0)
        if len(samples) < 3:
            samples.append((c["raw"], text))

    t = sorted(times)
    p = lambda q: t[min(int(len(t) * q), len(t) - 1)]
    print(json.dumps({
        "n": len(t),
        "p50_ms": round(p(0.50)), "p95_ms": round(p(0.95)),
        "min_ms": round(t[0]), "max_ms": round(t[-1]),
        "filler_removed": round(statistics.mean(fill), 3),
        "terms_kept": round(statistics.mean(keep), 3),
        "content_dropped": round(statistics.mean(dropped), 3),
        "invented": round(statistics.mean(invented), 3),
        "kind_correct": round(statistics.mean(kinds), 3),
        "schema_valid": f"{parsed}/{len(cases)}",
    }))
    for raw, text in samples:
        print(f"  in : {raw}\n  out: {text}")

main()
