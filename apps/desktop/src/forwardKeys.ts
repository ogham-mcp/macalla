// SPDX-License-Identifier: MIT
import { shortcutFocusLost, shortcutKey } from "./commands";

// On Windows the shortcut's keyboard hook hears nothing while a Macalla window
// has focus (a WebView2 window's keys never reach a low-level hook in its own
// process), so the window passes its own keys on, and the shortcut works here
// as it does in any other app. Capture phase, so nothing on the page can stop
// a key first; nothing is prevented, so typing is untouched. When the window
// loses focus, keys it still counts as down are let go unless they really are
// still held, or the shortcut could stay held for good.
export function forwardKeysOnWindows(): void {
  if (!navigator.userAgent.includes("Windows")) {
    return;
  }
  const down = new Set<string>();
  const send = (code: string, isDown: boolean, repeat: boolean) => {
    shortcutKey(code, isDown, repeat).catch(() => undefined);
  };
  window.addEventListener(
    "keydown",
    (e) => {
      down.add(e.code);
      send(e.code, true, e.repeat);
    },
    true,
  );
  window.addEventListener(
    "keyup",
    (e) => {
      down.delete(e.code);
      send(e.code, false, false);
    },
    true,
  );
  const letGo = () => {
    if (down.size > 0) {
      shortcutFocusLost([...down]).catch(() => undefined);
      down.clear();
    }
  };
  window.addEventListener("blur", letGo);
  // A window hidden with a key down may not be told it lost focus.
  document.addEventListener("visibilitychange", () => {
    if (document.hidden) {
      letGo();
    }
  });
}
