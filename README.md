# voxterm

A full-screen voice assistant for the terminal.

- **Gemini Live** talks with you and searches the web.
- **Tasks** (opencode, over the Agent Client Protocol) make every change to your code, several at once.

The assistant has no file or shell tools. Changes always go through tasks.

## Setup

```sh
export GEMINI_API_KEY=...      # read from the environment only
brew install meson ninja       # needed to build WebRTC echo cancellation
cargo install --path .         # installs `voxterm`
cd your-project && voxterm     # tasks run in the current directory
```

Requires `opencode` on your PATH. Run `opencode acp` once yourself to check it starts.

Headless check: `voxterm --selftest` runs a task and a Live request.

Live barge-in test (network, macOS `say`): `cargo test barge_in -- --ignored --nocapture`.

Debug log: `VOXTERM_LOG=/path/to/log voxterm` records UI events, mic state, and playback.

Everything is Rust. The terminal UI is ratatui, and the core runs in the same process.

## Keys

No function keys. Type to talk. Commands run from the palette or as slash words.

| Key | Action |
|---|---|
| Enter | Send the message line |
| ctrl-k | Command palette. Lists every command and its shortcut |
| `/` on an empty line | Palette, filtered by what you type. `/voice`, `/model`, `/task-model`, `/mute`, `/unmute`, `/theme <name>`, `/save`, `/cancel-all`, `/quit`, `/help` |
| ctrl-t | Mute or unmute the microphone |
| Tab, Shift-Tab | Move focus: conversation, tasks, message line |
| Up, Down, Enter (tasks focus) | Select a task. Enter opens its log |
| ctrl-c (tasks focus, task running) | Cancel the selected task |
| ctrl-c | Quits. Asks once first if tasks are running |
| ctrl-q | Quit |
| y / n, Esc | Allow or deny a pending task change. y and n work only while the message line is empty |
| ctrl-u | Clear the message line |
| Space (voice picker) | Preview the highlighted voice |

## Themes

Six colour themes from btop: `default`, `dracula`, `nord`, `gruvbox`, `tokyo-night`, `solarized`. Pick one with `/theme <name>`, or cycle with the palette entry. The choice is saved in `~/.config/voxterm/ui.json`.

## Behavior

- **Header state:** green = listening, magenta = speaking, yellow = muted, red = retrying, cyan = connecting.
- **Mute on blur** drops the mic stream when the terminal loses focus. This turns off the macOS mic indicator.
- **Echo cancellation** (WebRTC AudioProcessing) runs on the mic. It cancels the speaker output the app played. Its benefit on built-in speakers is not yet measured.
- **Echo gate** (`echo_gate`, default off). Set to `true` to block the mic while the assistant speaks. The gate also blocks barge-in.
- **Barge-in** through the mic is not yet verified end to end. Live interrupts on speech in a test with synthetic speech. The app path through a real mic and speakers is untested.
- **Ambient speech** from a TV or people nearby starts turns. In testing, the false turns matched TV dialogue, not the assistant's words. Use `mic_min_rms` to reject quiet room noise.
- **Mic level gate** (`mic_min_rms`, default 0) drops mic chunks quieter than this RMS. Set it from the `mic rms` values in `VOXTERM_LOG`. Room noise in testing read about 0.02 RMS. Speech is louder.
- **Voice approvals** cover file edits only. Shell and other actions need a screen confirm.
- **Settings** live in `~/.config/voxterm/config.json`.
- **Task model** picks apply to new tasks. A restart uses the worker's default.
- Changing voice or Live model reconnects the assistant. The conversation context resets.

## Known limits

- Audio needs a 32-bit float device. Other formats are reported as errors.
- Resampling is linear, not a sinc filter.
- macOS terminal apps need microphone permission. Without it, the mic is silent and voxterm warns you.
