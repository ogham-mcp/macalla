// SPDX-License-Identifier: MIT
//
// The icons this embeds (see `tauri.conf.json`'s `bundle.icon`, and
// `icons/`) are a placeholder: a flat-colour 1024x1024 PNG run through
// `npm run tauri icon`, not final artwork. tauri.conf.json is strict JSON
// with no comment syntax, so the note lives here instead.
//
// The app's commands go to tauri-build's app manifest. Without it, every
// command registered with `invoke_handler` may be called from every window,
// the overlay included; with it, a window may call a command only when one of
// its capabilities (`capabilities/`) grants the command's `allow-` permission.
// See Tauri's documentation on capabilities and the app manifest.
include!("src/app_commands.rs");

fn main() {
    tauri_build::try_build(
        tauri_build::Attributes::new()
            .app_manifest(tauri_build::AppManifest::new().commands(APP_COMMANDS)),
    )
    .expect("tauri-build failed");
}
