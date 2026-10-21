// SPDX-License-Identifier: MIT
//
// Dev-only: plays the overlay's events in a browser, so its states can be
// looked at with `npx vite` and no Tauri app running. Imported only from
// overlay.tsx, only when `import.meta.env.DEV` is true and the page URL has
// `?preview` -- see ./README.md. Never imported by a release build.
//
// `overlay.html?preview&loading=open` (the default) or `&loading=reload`:
// a press waiting on a model loading, the way the dictation thread sends it
// (an open's `dictation-state: preparing` first), then, after `&for=`
// milliseconds (3000 by default), the open's `idle` and, a second later,
// the press's `listening`. `&for=0` stays on the loading state.

import "./mockTauri";
import { emit } from "@tauri-apps/api/event";
import type { EngineLoading } from "../commands";

const params = new URLSearchParams(window.location.search);
const reason: EngineLoading["reason"] = params.get("loading") === "reload" ? "reload" : "open";
const holdFor = Number(params.get("for") ?? "3000");

// Long enough for the overlay's `listen` calls, made as it mounts, to be in.
window.setTimeout(() => {
  if (reason === "open") {
    void emit("dictation-state", "preparing");
  }
  void emit("dictation-loading", { engine: "Parakeet", reason } satisfies EngineLoading);
  if (holdFor > 0) {
    // The open's `idle`, and a second later -- drawn out, so the overlay
    // can be seen to keep the loading line through it -- the press's
    // `listening`.
    const listening = reason === "open" ? 1000 : 0;
    window.setTimeout(() => {
      if (reason === "open") {
        void emit("dictation-state", "idle");
      }
      window.setTimeout(() => void emit("dictation-state", "listening"), listening);
    }, holdFor);
  }
}, 300);
