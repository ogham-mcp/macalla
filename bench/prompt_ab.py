#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
"""Score Tier 1 prompt candidates on what Tier 1 actually receives.

The earlier bake-off fed raw transcripts to the model. Tier 0
comes first in the pipeline, so that measured a pipeline we do not ship. This scores each
prompt on Tier 0 OUTPUT, and asks the only question that matters: does the
model improve the text, or damage it?
"""
import json, re, statistics, sys, time, urllib.request

PORT = 8899
GRAMMAR = open('bench/block.gbnf').read()

def norm(t):
    return [w for w in re.split(r'[^a-z0-9]+', t.lower()) if w]

def dropped(src, out):
    s, pool, kept = norm(src), norm(out), 0
    if not s: return 0.0
    for w in s:
        if w in pool:
            pool.remove(w); kept += 1
    return 1 - kept / len(s)

def invented(src, out):
    o, pool, new = norm(out), norm(src), 0
    if not o: return 0.0
    for w in o:
        if w in pool: pool.remove(w)
        else: new += 1
    return new / len(o)

def ask(system, user):
    body = json.dumps({
        "messages": [{"role": "system", "content": system},
                     {"role": "user", "content": user}],
        "grammar": GRAMMAR, "temperature": 0.0, "n_predict": 200, "cache_prompt": True,
    }).encode()
    t0 = time.perf_counter()
    with urllib.request.urlopen(urllib.request.Request(
            f"http://127.0.0.1:{PORT}/v1/chat/completions", body,
            {"Content-Type": "application/json"}), timeout=180) as r:
        raw = json.load(r)["choices"][0]["message"]["content"]
    ms = (time.perf_counter() - t0) * 1000
    try: return ms, json.loads(raw).get("text", "")
    except Exception: return ms, raw

def main():
    cases = json.load(open('bench/tier0-out.json'))
    prompts = json.load(open('bench/prompts.json'))["prompts"]
    print(f"{'prompt':<18}{'p50 ms':>8}{'dropped':>9}{'invented':>10}{'punctuated':>12}{'damaged':>9}")
    results = {}
    for name, system in prompts.items():
        ask(system, "warm up")
        times, drops, invs, punct, damaged, samples = [], [], [], 0, 0, []
        for c in cases:
            src = c["tier0"]
            ms, out = ask(system, src)
            times.append(ms)
            d, i = dropped(src, out), invented(src, out)
            drops.append(d); invs.append(i)
            # Did it do the one job it is here for?
            if re.search(r'[.,?!;]', out) and not re.search(r'[.,?!;]', src):
                punct += 1
            if d > 0.001 or i > 0.001:
                damaged += 1
                if len(samples) < 2: samples.append((src, out))
        t = sorted(times)
        results[name] = dict(p50=t[len(t)//2], dropped=statistics.mean(drops),
                             invented=statistics.mean(invs), punct=punct, damaged=damaged)
        r = results[name]
        print(f"{name:<18}{r['p50']:>8.0f}{r['dropped']:>9.3f}{r['invented']:>10.3f}"
              f"{f'{punct}/{len(cases)}':>12}{f'{damaged}/{len(cases)}':>9}")
        for s, o in samples:
            print(f"    src: {s}\n    out: {o}")
    json.dump(results, open('bench/prompt-results.json','w'), indent=2)

main()
