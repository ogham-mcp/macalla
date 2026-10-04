#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
"""Does Tier 1 add anything to REAL engine output?

Every Tier 1 evaluation so far ran on hand-written unpunctuated fixtures. Apple
SpeechTranscriber already punctuates and capitalises, so those measured a job
the model does not actually have. This runs the real chain -- engine text, then
Tier 0, then Tier 1 -- and asks what is left for the model to contribute.
"""
import json, re, subprocess, sys, urllib.request

GRAMMAR = open('bench/block.gbnf').read()

def norm(t):
    return [w for w in re.split(r'[^a-z0-9]+', t.lower()) if w]

def diff_counts(src, out):
    s, o = norm(src), norm(out)
    pool = list(o); kept = 0
    for w in s:
        if w in pool: pool.remove(w); kept += 1
    dropped = 1 - kept / len(s) if s else 0.0
    pool = list(s); new = 0
    for w in o:
        if w in pool: pool.remove(w)
        else: new += 1
    return dropped, (new / len(o) if o else 0.0)

def ask(system, user):
    body = json.dumps({
        "messages": [{"role":"system","content":system},{"role":"user","content":user}],
        "grammar": GRAMMAR, "temperature": 0.0, "n_predict": 200, "cache_prompt": True}).encode()
    with urllib.request.urlopen(urllib.request.Request(
            "http://127.0.0.1:8899/v1/chat/completions", body,
            {"Content-Type":"application/json"}), timeout=180) as r:
        raw = json.load(r)["choices"][0]["message"]["content"]
    try: return json.loads(raw).get("text","")
    except Exception: return raw

cases = [c["engine"] for c in json.load(open('bench/engine-fixtures.json'))["cases"]]
prompts = json.load(open('bench/prompts.json'))["prompts"]

# Tier 0 on real engine text.
t0 = json.loads(subprocess.run(
    ["./target/debug/tier0text"], input=json.dumps(cases),
    capture_output=True, text=True, check=True).stdout)

t0_changed = sum(1 for a, b in zip(cases, t0) if norm(a) != norm(b))
print(f"Tier 0 changed {t0_changed}/{len(cases)} real engine outputs")
for a, b in zip(cases, t0):
    if norm(a) != norm(b):
        print(f"  engine: {a}\n  tier0 : {b}")

print(f"\n{'prompt':<18}{'changed':>9}{'dropped':>9}{'invented':>10}{'damaged':>9}")
for name, system in prompts.items():
    ask(system, "warm up")
    ch = dr = iv = dmg = 0
    samples = []
    for src in t0:
        out = ask(system, src)
        d, i = diff_counts(src, out)
        dr += d; iv += i
        if norm(src) != norm(out):
            ch += 1
            if d > 0.001 or i > 0.001:
                dmg += 1
                if len(samples) < 2: samples.append((src, out))
    n = len(t0)
    print(f"{name:<18}{f'{ch}/{n}':>9}{dr/n:>9.3f}{iv/n:>10.3f}{f'{dmg}/{n}':>9}")
    for s, o in samples:
        print(f"    in : {s}\n    out: {o}")
