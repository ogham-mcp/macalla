# How to use Macalla

Macalla turns what you say into text in whatever app you are typing in. This
page covers the desktop app on macOS: dictating, what the small window at the
bottom of the screen tells you, and the settings that decide where your words
go.

## Before the first dictation

Macalla needs three macOS permissions, and a checklist walks you through them
when it first opens:

- **Microphone**, to hear you.
- **Input Monitoring**, to notice when you hold the shortcut.
- **Accessibility**, to paste into other apps.

If you turned one down, macOS will not ask again on its own. Turn it on in
System Settings, in the pane the checklist names.

## Dictating

1. Put the cursor where you want the text.
2. Hold **Right Option** and speak.
3. Let go. The text is pasted where the cursor is.

To throw a dictation away, click the **×** at the right of the overlay, or
choose **Cancel Dictation** from the menu bar icon, before the text is pasted.
Nothing is pasted and nothing is kept. The **×** is not shown while you hold
the shortcut: let go first, or use the menu.

A single dictation can run for up to 5 minutes. If you keep talking past that,
Macalla stops and says "Stopped at the 5-minute limit. Anything after it was
not heard."

You can also start a dictation from the menu bar icon, with **Start
Dictation**. While a dictation runs, that item reads **Stop Dictation**, and
**Cancel Dictation** appears below it. To change the shortcut, open the
**Shortcut** card in Settings and pick another key from the list, or choose
**Record a key…** and press the key you want. The shortcut is a modifier key
held on its own — such as Shift or Control, or on macOS Option, Command or Fn.
Macalla uses a single held key, rather than a typed combination, so the
shortcut keeps working everywhere — even while you type a password, where the
system hides ordinary keys from apps. Combinations such as Option with Shift
are planned for macOS. The menu's **Microphone** submenu lists "System default" and then every microphone
connected, and ticks the one in use; choosing another there does the same
thing as the Microphone card in Settings.

## What the overlay tells you

While you speak, a small window at the bottom of the screen shows a waveform
and the words Macalla has heard so far. Each bar turns amber when you are
loud, and red when you are close to distorting; if you see red, move back
from the microphone or turn its input level down. The **×** at the right of
the waveform throws the dictation away, until the text is pasted. While
Macalla is still listening, a teal **✓** beside it stops the dictation and
pastes the text, as **Stop Dictation** does. Neither shows while you are
holding the shortcut key; the ✓ is for a dictation started from the menu bar.

The right of the bottom row names who your words are sent to: the cloud
transcription service, if you use one, and the provider that tidies your text,
if Enhancement is on. With several cloud models ticked, it names the first
provider and counts the others, such as "OpenAI +1". If something you chose
can't be used, the label says so, for example "Parakeet · OpenAI needs a key":
Parakeet is hearing you, and OpenAI tidies nothing until you add its key. With
transcription on your Mac and Enhancement off, the right-hand side is empty.
The label does not tell you whether the pasted text was already tidied.

Other messages you may see in the overlay:

- **"No speech detected."** Macalla heard nothing it could turn into words, so
  nothing was pasted.
- **"Waiting for {provider} (N s)"**, for example "Waiting for ElevenLabs
  (3 s)". You use a cloud transcription service, and Macalla is waiting for its
  answer. If History is on, it may add "What this Mac heard is in History."
  If you start another dictation now (hold **Right Option** again, or choose
  **Start Dictation**), Macalla still waits for the answer, then pastes it
  without waiting for Enhancement and starts the next one. With History on,
  Enhancement still tidies it, and the tidied text replaces it in History. With
  History off, Enhancement is skipped for it.
- **"Enhancing…"**, shown on the left of the bottom line. Enhancement is on,
  and Macalla waits up to 5 seconds for the tidied text before it pastes, and
  your words stay on screen while it waits. If you start another dictation now,
  this one is pasted straight away without Enhancement, and the next one
  starts. If its tidied text arrives, it replaces the pasted text in History,
  if History is on.
- **"Pasted without Enhancement."** Enhancement did not finish within 5
  seconds, or it failed, so your words were pasted without waiting for it. If
  the tidied text is still on its way and History is on, it says "Pasted
  without Enhancement. If it finishes, History will have the enhanced text."
  It can still time out or fail, and then History keeps what was pasted. If
  the dictation also stopped at the 5-minute limit, the overlay says that
  instead, and the tidied text still replaces it in History if it arrives.
- **"Not pasted, because another app is in front. It is in History."** You
  switched to another app while Macalla was waiting for a cloud service or for
  Enhancement, so it did not paste into the wrong place. Clicking a Macalla
  window, such as Settings, during the dictation and leaving it in front counts
  as switching. If History is off, the second sentence says it was not kept.

Macalla pastes through the clipboard and puts back what you had on it
afterwards. If it can't paste, it types the text instead, which is slower.

If the text could not be pasted at all, Macalla says "This text could not be
pasted. Try again."

## Transcription: what hears you

Transcription turns your speech into text. Choose it on the **Models** page.

- **Apple Speech** runs on your Mac and is the default. It needs no download.
- **Downloadable models** also run on your Mac: Parakeet, Nemotron and
  Whisper. Each is downloaded once from the Models page.
- **Cloud services** (OpenAI, Gemini, Mistral, OpenRouter, ElevenLabs and
  Cohere) send your recording to that company. They are off until you add your
  own key for the provider under **Providers**. Keys stay in your Mac's
  Keychain and are never shown again.

A downloaded model is unloaded after 5 minutes without a dictation, to free
its memory, and loaded again at the next one. To change the time or keep the
model loaded, use **Unload model** on the Engine card. If you press the
shortcut while a downloaded model loads, the overlay says so. Start speaking
when it says Listening. The first load on a Mac can take ten seconds or more.

## Enhancement: what tidies your words

After the dictionary has fixed your words, Macalla's own tidying runs: rules
that remove fillers such as "um", collapse a stutter into one word, and turn
spoken punctuation such as "comma" into the mark. Enhancement can then tidy
punctuation and capitals with a language model. It is optional. Choose it on
the **Enhancement** card:

- **Off**: only the dictionary and Macalla's own tidying. Nothing is sent
  anywhere.
- **Apple Intelligence**: tidies your words on this Mac.
- **Ollama or another server**: a server on this Mac, or on another computer
  on your own network, tidies your words.
- **Cloud models**: models from the providers you have saved a key for. Your
  words are sent to them.

With cloud models you can tick more than one, then press **Use these models**.
They are asked in the order you ticked them: the first is asked first, and if
it fails, the next takes over. To change the order, remove a model and tick it
again. The card then shows which models are in use, such as "In use: OpenAI
gpt-5.4-mini, falling back to OpenAI gpt-5.4-nano".

With Enhancement on, Macalla waits up to 5 seconds for the tidied text before
it pastes, and the overlay says "Enhancing…" on its bottom line. If
Enhancement has not finished by then, or it fails, your words are pasted
without it. When it finishes later, it replaces that dictation's text in
History, if History is on. With Enhancement off, your words are pasted
straight away.

Apple Intelligence and a server on this Mac tidy your words while you speak.
Your own server on another computer, or a cloud model, is sent your words at
most once, when they are ready: when you let go of the key, or when your cloud
transcription service answers. If you change Enhancement during a
dictation, a server on another computer or a cloud model is sent nothing more
for it, while Apple Intelligence and a server on this Mac finish the dictation
they started.

## History

History keeps what you dictated, in a file on this Mac. It is never sent
anywhere. Audio is never saved.

Each entry shows the final text: after the dictionary, Macalla's own tidying
and Enhancement, if it's on. When speech recognition's own words were different, they
appear underneath as "Heard:". That is what was heard before any of those
changes, not the text Enhancement was given.

Choose how long History keeps things: until Macalla quits, for 1, 7 or 30
days, or not at all. Thirty days is the default and the longest. Turning
History off erases what was kept before.

If a cloud transcription service failed to answer, the latest dictation's row
offers **Send again**. It works only until your next dictation starts, because
Macalla lets the recording go then. Starting a dictation while Send again is
waiting stops that wait: the new dictation goes ahead, and the row keeps what
this Mac heard.

## Files

The **Files** page works on a whole meeting rather than a dictation. Open an
audio recording to have it transcribed, or open a transcript you already have
(`.txt` or `.vtt`). Then you can tidy it with your Enhancement choice,
and summarise it into key points, decisions and action items. Copy the result,
or save it as text to your Downloads folder.

A file is kept in memory only, until you clear it or quit Macalla. Nothing
from the Files page is written to History. One file is worked on at a time.

## Live meetings

A live meeting turns a call, or a meeting in the room, into text while it
happens. Open the **Files** page and choose **Start a live meeting**.

Macalla hears two things at once. Your microphone is shown as **You**, and the
sound your Mac plays, such as the other side of a video call, is shown as
**Others**. The meeting is heard by the speech engine on this Mac, even when
you have chosen a cloud provider for dictation. The sound is heard as it
arrives and is never saved.

**The first time.** macOS asks whether Macalla may record the sound your Mac
plays, with the reason "To hear the other side of a call you record as a
meeting." Allow it. If you declined, turn it on in System Settings, under
Privacy & Security. We have not yet confirmed the exact name of that pane on
every version of macOS, so look for Macalla there.

**While it records**, the page shows how much it has heard and whether each
side is coming through. If the call has made no sound after 15 seconds, the
page says so and points you to that permission. If Macalla cannot open the
sound your Mac plays, the meeting hears your microphone alone, and the page
tells you.

Dictation is off while a live meeting records, and **Send again** in History
says to stop the meeting first. Both work again as soon as the meeting ends.

**Ending it.** Choose **Stop and keep** to keep what was heard, or **Discard**
to throw it away. A kept meeting opens on the Files page as a file does, so
you can tidy it, summarise it, copy it, or save it as text to your Downloads
folder. Nothing from a live meeting is written to History.

**If Macalla quits while a meeting records**, it writes what was heard to a
file on this Mac as it closes, and offers it back on the Files page the next time Macalla opens. It stays there
until you copy it, save it or clear it. If Macalla could not finish hearing the
last part before it closed, the page says the last part may be missing. When
more than one meeting was held this way, they come back one at a time, oldest
first.

### From the command line

`macalla live-meeting` records from your microphone only. To record the other
side of a call, use the desktop app: see
[Known behaviours](known-behaviours.md).

- Press **Enter** or **Ctrl-C** to stop and print what was heard.
- Press **Ctrl-C** a second time to throw the meeting away instead, and a
  third time to end the program at once.
- A script or an agent stops the meeting and keeps it by sending `SIGINT`,
  as Ctrl-C does. `SIGTERM` also stops and keeps it.
- Add `--json` for the turns as JSON, and `--device` to choose a microphone
  by the name `macalla devices` lists.

## Dictionary

The **Dictionary** page holds words Macalla should get right, such as product
names and technical terms. For each term you can list what speech recognition
tends to hear instead, under **Heard as**: "Macalla" can be heard as "Makala",
for example. Macalla then replaces what it heard with the term. It also catches a close mishearing of a term, but only
when both the sound and the spelling are a near match.

## General

The **General** card, at the bottom of the settings, holds **Open Macalla when
you log in**. It is off until you turn it on.

When it's on, Macalla starts each time you log in and waits in the menu bar,
without opening a window. If setup isn't finished yet, the checklist opens
instead, as it does on any launch.

The setting is a file in `~/Library/LaunchAgents`. Turning the switch off
removes it.

## When something looks wrong

[Known behaviours](known-behaviours.md) lists things you may notice, why they
happen, and what to do about them.
