# Known behaviours

Things you may notice while using Macalla, what causes them, and what to do
about them. Most come from your hardware or the operating system rather than
from Macalla itself; a few come from the fixed rules Macalla uses to tidy your
words. Written so each entry can be lifted into the website's questions page as
it stands. Every figure states the machine it was measured on.

## Apple Intelligence leaves most of a long Chinese, Japanese, Korean or Vietnamese meeting unsummarised

**What you notice.** You ask for a summary of a meeting held in Chinese,
Japanese, Korean or Vietnamese, with Apple Intelligence chosen on the
Enhancement card. Most of the summary is missing: any part of the meeting longer than
about 1,900 characters can come back with an error, and one longer than about
3,800 always does. A short meeting, or a short part at the end of a long one,
is still summarised.

**Why it happens.** Apple's on-device model can take 4096 tokens at a time,
counting the instructions, the transcript and its reply together. That is
Apple's figure, not a measurement of ours. The instructions leave about 3,800
tokens. Macalla plans to use about half of those, about 1,900, for each part of
the transcript and leave the rest for the reply, so it cuts a meeting into
parts of up to about 5,750 characters: about 1,900 tokens where a token is
three or four characters, as Apple describes English. In Chinese, Japanese,
Korean and Vietnamese a token is about one character, so the same part is
nearer 5,750 tokens. A part longer than about 3,800 characters cannot fit at
all, even before the reply. One longer than about 1,900 characters fits only if
the reply is short, and fails whenever the reply needs more than the room
left. Counting each part's tokens before sending it would fix this, and is
planned for a later phase.

**What to do.** For these meetings, choose a cloud model with a larger context
on the Enhancement card. It summarises them in parts of the same size.

## A phrase you repeat on purpose can lose a copy

**What you notice.** You say a short phrase more than once on purpose, such as
counting "one two three one two three one two three", and the pasted text has
one copy fewer: "one two three one two three".

**Why it happens.** When people restart a phrase they often say it twice, as in
"we need to we need to ship", and Macalla removes the extra copy. From the words
alone it cannot tell a stumble from a repeat you meant, so it treats every
repeat the same way:

- A run of one to three words said again straight away is removed in pairs: two
  copies become one, three become two, and four usually become two.
- A single word said twice is removed only if it is a short everyday word, such
  as "the", "to", "it" or "we". "very very good" and "azure azure" stay as you
  said them.
- The copies must follow each other directly. A comma, dash or full stop between
  them keeps every copy.

**What to do.** When a repeat matters, put punctuation between the copies, for
example by saying "comma" after each one, or check the pasted text.

## Bluetooth headset microphones make playback thinner while you dictate

**What you notice.** You are listening to music or a podcast through Bluetooth
headphones, such as AirPods, and they are also the microphone Macalla listens
through. When you start dictating, the audio cuts out for a moment and comes
back thinner, sometimes a little louder. About three seconds after you finish,
it returns to normal.

**Why it happens.** A Bluetooth headset has two ways of working. For playback
it uses a high-quality stereo mode. As soon as any app records from its
microphone, the headset switches to a headset mode that carries your voice and
the audio at the same time, at lower quality. It switches back once recording
stops. This is how Bluetooth headsets work. Another dictation app we tested
behaves the same way, and we know of no way to avoid it while the headset's own microphone
is recording. Macalla is open source: if you find one, an issue or a pull
request is welcome.

**What to do.** Keep the headphones as your output and choose your Mac's
built-in microphone, or another wired or USB microphone, on Macalla's
Microphone card. Playback then stays as it is while you dictate.

**What we measured.** On a MacBook Pro (Apple M1 Pro), macOS 26.6.2, with
AirPods Pro, on 12 September 2026:

- Recording from the AirPods microphone switched their playback from 48,000 Hz
  to 24,000 Hz within a fifth of a second of recording starting.
- After recording stopped, playback was back at 48,000 Hz about two seconds
  later, and audio resumed about three seconds later.
- With the MacBook's built-in microphone chosen instead, playback stayed at
  48,000 Hz throughout.
- Another dictation app recording from the same AirPods produced the same
  switch.

While the headset is in that mode its microphone also runs at 24,000 Hz.
Macalla converts every microphone to 16,000 Hz before recognition, so that rate
is enough for the speech engine. Whether Bluetooth's compression of the
microphone audio changes how accurately your words are recognised has not been
measured.

## Without headphones, the other side of a call can appear under You as well

**What you notice.** In a live meeting on a call, some of what the other
people said appears twice: once under **Others**, and again under **You**.

**Why it happens.** A live meeting hears your microphone and the sound your Mac
plays as two separate streams. When the call plays through your Mac's
speakers, your microphone hears it too, so the same words reach both streams.
Macalla does not remove the repeats, because text that repeats what someone
else said can also be something you really said, such as reading a point back
to agree to it. Removing it would delete your words without telling you. How
often this happens, and with which microphones, has not been measured.

**What to do.** Wear headphones for calls you record as a live meeting. The
call then reaches Macalla only as **Others**.

## Disconnecting or switching headphones can end a live meeting early

**What you notice.** A live meeting stops on its own and the Files page says
why: for example that a device went away, or that the call's sound changed,
as it does when headphones connect. The transcript up to that moment is kept.

**Why it happens.** Macalla hears each side through a device, and when a device
disappears, or the sound your Mac plays changes format, it cannot go on
hearing that side correctly. Rather than carry on with audio it can no longer
trust, it ends the meeting and keeps everything it had already heard. A short
break in the sound, too brief to change anything, is filled with silence
instead, so the meeting carries on and each side keeps its place in time.

**What to do.** Choose your microphone and headphones before you start a
meeting, and leave them connected until you stop it. If a meeting ends early,
start a new one: the text you already have stays on the Files page until you
copy, save or clear it.

**What we saw.** On a Mac mini (Apple M1), macOS 27.0, on 23 September 2026:
disconnecting AirPods during a live meeting ended it, and the words heard
before the disconnect were kept.

## Over a long live meeting, the two sides' timings can drift slightly apart

**What you notice.** Late in a long meeting, a reply under **Others** can
appear a moment before or after the words under **You** that it answers.

**Why it happens.** Your microphone and the sound your Mac plays are timed by
two different clocks, and no two clocks run at exactly the same speed. Over a
long meeting the small difference adds up. Macalla does not correct for it.
How far apart the two sides drift has not been measured.

**What to do.** Nothing needs doing: the words are unaffected, only the order
of two turns that are close together. When a reply looks out of place late in
a long meeting, read it with the turn next to it.

## A live meeting from the command line hears only your microphone

**What you notice.** `macalla live-meeting` records you, but not the other side
of a call playing on your Mac.

**Why it happens.** macOS decides who may record the sound your Mac plays by
the app that started the program. For a command run in a terminal, that is
the terminal, and a terminal is not set up to ask for that permission, so
macOS gives it silence. The command says it records your microphone only.

**What to do.** Record calls from the **Files** page of the desktop app, which
asks for the permission itself.

**What we saw.** On a Mac mini (Apple M1), macOS 27.0, on 21 September 2026: a
program run from a terminal received the sound the Mac was playing as silence,
and macOS's own log showed it refusing the terminal, which had not asked for
the permission.

## Quitting while a live meeting records can end in a crash report

**What you notice.** You quit Macalla during a live meeting. The next time it
opens, the meeting is offered back on the Files page as expected, but macOS
may also have shown a report that Macalla quit unexpectedly. We have not seen
this happen: it is worked out from the code.

**Why it happens.** When you quit, Macalla waits a few seconds for the meeting
to finish hearing its last piece, and saves what was heard before it closes.
If that piece is still being heard when the time is up, the speech engine can
still be running as Macalla closes, and that can end in the crash report. What
was heard is already saved by then.

**What to do.** Stop the meeting before you quit. Then nothing is still being
heard when Macalla closes. If you do see the report, the meeting is still
offered back the next time Macalla opens.

## Dictating while several files are being heard can be slower

**What you notice.** You drop several files on the Files page and dictate
while they are being heard. The dictation takes longer than usual to appear.

**Why it happens.** Macalla hears the files on the same speech engine that
dictation uses, rather than loading a second copy of it, so a dictation can
wait behind the part of a file the engine is hearing. We have not measured how
much longer it takes.

**What to do.** Wait for the files to finish, or press Stop on the Files page.
Files already saved stay saved.

## Changing the speech engine while several files are being heard

**What you notice.** You change the speech engine while the Files page is
hearing several files. The recordings that were still waiting are not heard,
and each says the engine was changed.

**Why it happens.** A batch of files is heard on the one engine it started
with, so none of its files is heard on a different engine without a word. A
batch never loads a second engine of its own; the file being heard when you
switch finishes on the old one. Transcript files, which need no engine, are
still read and saved.

**What to do.** Choose or drop the recordings that were not heard again, and
they are heard on the new engine.

## Apple Speech can hear a technical name differently each time

**What you notice.** Apple's speech engine writes an unfamiliar name, such as
Kubernetes, a little differently from one dictation to the next, and it does
not take hints from your dictionary.

**Why it happens.** The dictionary fixes a name only when it has seen that
spelling before, so some get through.

**What to do.** Parakeet, chosen on the Engine card, hears technical names more
consistently. You can also add a mishearing to the name on the Dictionary page
whenever you see one.
