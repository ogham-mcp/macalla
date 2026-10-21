# Browser preview (dev only)

A way to work on the settings UI in an ordinary browser, without the Tauri
app running. **Dev only -- never present in a release build**, see "Leak
guard" below.

## Run it

```sh
cd apps/desktop && npx vite
```

Then open <http://localhost:1420/?preview> (the port is fixed by
`vite.config.ts`'s `server.port`, not Vite's usual 5173). The main settings
page, the Models page, the History page, the Dictionary page and the Files
page render with realistic fake data.

The overlay has one too, at <http://localhost:1420/overlay.html?preview>:
`overlayPreview.ts` plays the events of a dictation waiting on a model
loading, `&loading=open` (the default) or `&loading=reload`, then, after
`&for=` milliseconds (3000 by default; `0` stays put), the events that
start it listening. Size the browser to the overlay's 360 by 96 to see it
as it is drawn.

Without `?preview`, or outside a dev server (`import.meta.env.DEV` false),
the app behaves exactly as it does today: the first Tauri call throws
"Cannot read properties of undefined (reading 'invoke')".

## What is mocked

`mockTauri.ts` intercepts IPC with `@tauri-apps/api/mocks`' `mockIPC`
(`shouldMockEvents: true`, so `listen()` calls resolve instead of hitting the
"not in preview" path) and `mockWindows("main")` (`App.tsx` calls
`getCurrentWebview()` for the Files page's file drops).

Covered: every command the main settings page, the Models page, the History
page, the Dictionary page and the Files page call when they load --
`shortcut`, `setup_steps`, `dictionary_info`, `history_retention`,
`input_device`, `microphones`, `models_overview`, `models_list`,
`models_dir`, `history_list`, `dictionary_terms`, `meeting_current`,
`meeting_destinations`, `meeting_running_summary`, `meeting_live_words`,
`meeting_running_live`, `meeting_batch`, and the translation card's
`meeting_translation_options`, `meeting_translation_check`,
`meeting_translation` and `meeting_running_translation`.

The translation card's state comes from the URL: `?preview&translation=`
`available` (the default), `not-installed`, `unsupported`, `unavailable` or
`done`, as `mockTauri.ts` describes.

The onboarding checklist's platform and state come from the URL too:
`?preview&os=` `macos` (the default), `windows` or `linux`, and `&setup=`
`done` (the default) or `pending`, as `mockTauri.ts` describes. With
`&engine=loading` the speech engine is still opening when the Files page first
asks and opens two seconds later, followed by `dictation-state: idle`, so the Files
page's engine line can be watched catching up.
With `&meeting=early-summary` the Files page opens while a summary runs: the
summary's `meeting-summary` event arrives at 300 ms, before the page's first
`meeting_current` reply at 1.2 s, which is the meeting without its summary.
The summary should stay on the page.

Not covered: anything reachable only from a click on one of those pages --
adding, editing, deleting or importing/exporting a dictionary term; copying,
deleting or resending a history item; choosing or opening a file, summarising
it, starting or ending a live meeting, or stopping any job. Those reject with an `Error` whose message starts
`not in preview: <command>`, so a card visibly says so instead of silently
doing nothing.

## Fixture data

`fixtures.ts` holds the fake data, each export typed with `satisfies
<Interface>` against `../commands.ts`, so a payload shape change fails `tsc`
here too. Nothing in it is real: paths use the shape
`~/Library/Application Support/dev.macalla.desktop/...`, written with `~` so
no fixture looks like a real home directory, no key is
a real key (only a `saved: true` flag), and model names/sizes are shaped
like `crates/macalla-engines/src/models.rs`'s real catalogue without copying
its licence text.

**Adding a fixture:** add a `case` to `mockTauri.ts`'s `mockIPC` switch and
an export in `fixtures.ts`, typed with `satisfies` against the matching
interface in `commands.ts`. That way a change to the real payload's shape
breaks the build here rather than silently drifting.

## Leak guard

Nothing above may reach a release build. Two things enforce that:

1. The `if (import.meta.env.DEV && ...)` guard in `main.tsx`, and the same
   one in `overlay.tsx` -- Vite strips a `false` `import.meta.env.DEV`
   branch (and therefore these modules) out of a production build entirely.
2. `scripts/check-no-preview.mjs`, run in CI (`ci.yml`'s `desktop ui` job)
   right after `npm run build`: it fails if the built `dist` contains
   `mockTauri.ts`'s marker string, or if any built file's name contains
   "preview". Run it yourself with `npm run check:no-preview`.
