// SPDX-License-Identifier: MIT
import React from "react";
import ReactDOM from "react-dom/client";
import App from "./App";
import { forwardKeysOnWindows } from "./forwardKeys";
import "./styles.css";

// Dev-only browser preview (see src/preview/README.md): with `?preview` on
// the URL in a dev server, stand in Tauri's IPC with fake data before
// anything calls it, so the settings UI can be worked on without the Tauri
// app running. `import.meta.env.DEV` is false in a release build, so this
// branch and everything it imports are gone from `dist` -- proven by
// scripts/check-no-preview.mjs, run in CI right after the build.
if (import.meta.env.DEV && new URLSearchParams(window.location.search).has("preview")) {
  await import("./preview/mockTauri");
}

forwardKeysOnWindows();

const root = document.getElementById("root");
if (!root) {
  throw new Error("missing #root element");
}

ReactDOM.createRoot(root).render(
  <React.StrictMode>
    <App />
  </React.StrictMode>,
);
