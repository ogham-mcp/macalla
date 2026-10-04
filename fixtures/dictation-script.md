# Dictation script

Read this aloud for `./bench/record-and-compare.sh`. It mirrors the synthesised
fixtures so a human recording is directly comparable to them.

**Read it badly on purpose.** The bracketed cues are the measurement — the
fillers, restarts and hesitations are the whole point. Do not tidy as you go.

---

[um] So the thing is, [uh] we need to — we need to ship the parser by Friday.

I ran Terragrunt against the Azure landing zone, and then Terraform applied the
VNet. [er] The HCL was valid.

[hmm] We deployed to AWS as well, and kubectl showed the pods running, [you know].

The Postgres database uses pgvector for the embeddings. Supabase runs on the
Proxmox LXC over Tailscale, [um], and Ogham exposes it through MCP.

Clear the cache before you count the cash. The queue took its cue from the sink
that failed to sync.

[so] The latency budget is three hundred milliseconds, [uh], and only the
trailing segment is in flight when you release the key.

---

## What each line is for

| Line | Tests |
|---|---|
| 1 | filler removal, and a genuine restart ("we need to — we need to") |
| 2–3 | technical vocabulary: Terragrunt, Azure, Terraform, VNet, HCL |
| 3 | `kubectl`, and a trailing "you know" |
| 4 | Postgres, pgvector, Supabase, Proxmox, LXC, Tailscale, Ogham, MCP |
| 5 | **homophones** — cache/cash, queue/cue, sink/sync |
| 6 | numbers, and a mid-sentence filler |

## Reading it

Say the bracketed words out loud as the sounds they are — "um", "uh", "er",
"hmm" — not as spelled-out letters. Pause naturally. If you fluff a word, keep
going: a real restart is better data than a clean take.
