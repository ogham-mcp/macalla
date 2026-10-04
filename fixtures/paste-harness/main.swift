// SPDX-License-Identifier: MIT
//
// The paste harness: a window that echoes whatever was pasted into it.
//
//   paste-harness <ready-file> <output-file> [timeout-seconds]
//
// Writes <ready-file> once the window is focused and able to receive a paste,
// then writes whatever text arrives to <output-file> and exits 0. Exits 2 if
// nothing arrives before the timeout.
//
// It exists because every other test of the paste path uses a fake clipboard
// and a fake key sender. Those prove the ORDER is right -- save, write, chord,
// wait, restore -- and prove nothing about whether text lands in another
// application, which is the only thing the user experiences.

import AppKit
import Foundation

let args = CommandLine.arguments
guard args.count >= 3 else {
    FileHandle.standardError.write(
        "usage: paste-harness <ready-file> <output-file> [timeout-seconds]\n".data(using: .utf8)!)
    exit(64)
}
let readyPath = args[1]
let outputPath = args[2]
let timeout = args.count > 3 ? (Double(args[3]) ?? 15.0) : 15.0

final class Harness: NSObject, NSApplicationDelegate, NSTextViewDelegate {
    let window: NSWindow
    let textView: NSTextView

    override init() {
        window = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 480, height: 120),
            styleMask: [.titled],
            backing: .buffered,
            defer: false
        )
        textView = NSTextView(frame: window.contentLayoutRect)
        super.init()
    }

    /// A real Edit menu, because that is what routes Cmd+V.
    ///
    /// AppKit does not translate the chord itself -- the Edit menu's Paste item
    /// does, via its key equivalent. A bare NSApplication has no menu bar, so
    /// Cmd+V arrives as a plain keyDown and nothing pastes. The harness looked
    /// broken for a while over this while the paste path underneath was fine.
    private func installEditMenu() {
        let mainMenu = NSMenu()

        let appItem = NSMenuItem()
        let appMenu = NSMenu()
        appMenu.addItem(withTitle: "Quit", action: #selector(NSApplication.terminate(_:)), keyEquivalent: "q")
        appItem.submenu = appMenu
        mainMenu.addItem(appItem)

        let editItem = NSMenuItem()
        let editMenu = NSMenu(title: "Edit")
        editMenu.addItem(withTitle: "Cut", action: #selector(NSText.cut(_:)), keyEquivalent: "x")
        editMenu.addItem(withTitle: "Copy", action: #selector(NSText.copy(_:)), keyEquivalent: "c")
        editMenu.addItem(withTitle: "Paste", action: #selector(NSText.paste(_:)), keyEquivalent: "v")
        editMenu.addItem(withTitle: "Select All", action: #selector(NSText.selectAll(_:)), keyEquivalent: "a")
        editItem.submenu = editMenu
        mainMenu.addItem(editItem)

        NSApp.mainMenu = mainMenu
    }

    func applicationDidFinishLaunching(_ notification: Notification) {
        installEditMenu()
        window.title = "Macalla paste harness"
        textView.isEditable = true
        textView.isRichText = false
        textView.delegate = self
        window.contentView = textView
        window.center()
        window.makeKeyAndOrderFront(nil)
        // The paste has to land in the text view, so it must be first
        // responder AND the app must be frontmost -- a synthetic Cmd+V goes to
        // whatever is focused, not to whoever asked for it.
        window.makeFirstResponder(textView)
        NSApp.activate(ignoringOtherApps: true)

        // Signal readiness only once this app is genuinely FRONTMOST. A
        // synthetic Cmd+V goes to whatever is focused, so announcing readiness
        // early does not merely fail the test -- it fires the paste into
        // whatever window the tester was looking at.
        waitUntilFrontmost(attemptsLeft: 40)

        // Watch every key event the app receives, so a failure can say whether
        // the chord arrived at all. "Nothing pasted" has two very different
        // causes -- the keystroke never got here, or it got here and did not
        // paste -- and they need opposite fixes.
        NSEvent.addLocalMonitorForEvents(matching: [.keyDown]) { event in
            let flags = event.modifierFlags.intersection(.deviceIndependentFlagsMask)
            FileHandle.standardError.write(
                "keyDown chars=\(event.charactersIgnoringModifiers ?? "?") cmd=\(flags.contains(.command))\n"
                    .data(using: .utf8)!)
            return event
        }

        DispatchQueue.main.asyncAfter(deadline: .now() + timeout) {
            let seen = self.textView.string
            let responder = self.window.firstResponder === self.textView
                || self.window.firstResponder is NSTextView
            FileHandle.standardError.write(
                ("nothing was pasted within \(timeout)s "
                    + "[active=\(NSApp.isActive) textViewIsFirstResponder=\(responder) "
                    + "editable=\(self.textView.isEditable) contents=\(seen.count) chars]\n")
                    .data(using: .utf8)!)
            exit(2)
        }
    }

    /// Poll until NSApp reports itself active, then signal. Gives up rather
    /// than signalling anyway.
    private func waitUntilFrontmost(attemptsLeft: Int) {
        if NSApp.isActive {
            try? "ready".write(toFile: readyPath, atomically: true, encoding: .utf8)
            return
        }
        guard attemptsLeft > 0 else {
            FileHandle.standardError.write(
                "never became frontmost; refusing to signal ready\n".data(using: .utf8)!)
            exit(3)
        }
        NSApp.activate(ignoringOtherApps: true)
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.1) { [weak self] in
            self?.waitUntilFrontmost(attemptsLeft: attemptsLeft - 1)
        }
    }

    func textDidChange(_ notification: Notification) {
        let received = textView.string
        guard !received.isEmpty else { return }
        // Give a multi-event paste a moment to finish before reading it, so a
        // slow insertion is not truncated into a false failure.
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.3) {
            try? self.textView.string.write(toFile: outputPath, atomically: true, encoding: .utf8)
            exit(0)
        }
    }
}

let app = NSApplication.shared
app.setActivationPolicy(.regular)
let harness = Harness()
app.delegate = harness
app.run()
