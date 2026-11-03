# voxterm

A full-screen voice assistant for the terminal.

- **Gemini Live** talks with you and searches the web.
- **A coding agent** (opencode, over the Agent Client Protocol) makes every change to your code.

The assistant has no file or shell tools. Changes always go through the coding agent.

## Setup

```sh
export GEMINI_API_KEY=...      # read from the environment only
cargo install --path .         # installs `voxterm`
cd your-project && voxterm     # the agent runs in the current directory
```

Requires `opencode` on your PATH. Run `opencode acp` once yourself to check it starts.

Headless check: `voxterm --selftest` runs an agent prompt and a Live request.

Barge-in test (network, macOS `say`): `cargo test barge_in -- --ignored --nocapture`.

Debug log: `VOXTERM_LOG=/path/to/log voxterm` records UI events, mic state, and playback.

## Keys

| Key | Action |
|---|---|
| Enter | Send typed text to the assistant |
| F2 | Mic mute or unmute |
| F3 | Voice picker. `p` previews the highlighted voice |
| F4 | Live model picker |
| F5 | Agent model picker |
| F6 | Toggle mute on blur |
| F7 | Save transcript to `voxterm-<time>.md` in the working directory |
| y / n | Approve or deny a permission request |
| ctrl-c | Cancel the agent. Quits when the agent is idle |
| ctrl-q | Quit |

## Behavior

- **Mute on blur** drops the mic stream when the terminal loses focus. This turns off the macOS mic indicator.
- **Echo gate** (`echo_gate`, default auto). Auto turns the gate on when the output device name contains "speaker". The gate stops the mic from sending while the assistant speaks. It also blocks barge-in.
- **Barge-in** works with headphones or any output not named "speaker". Built-in speakers need the gate on, so they cannot interrupt the assistant until echo cancellation exists.
- **Ambient speech** from a TV or people nearby can start a turn. Headphones help. Use `mic_min_rms` to reject quiet room noise.
- **Mic level gate** (`mic_min_rms`, default 0) drops mic chunks quieter than this RMS. Set it from the `mic rms` values in `VOXTERM_LOG`. Room noise in testing read about 0.02 RMS. Speech is louder.
- **Voice approvals** cover file edits only. Shell and other actions need a screen confirm.
- **Settings** live in `~/.config/voxterm/config.json`.
- **Agent model** picks apply to the current session only. A restart uses the agent's default.
- Changing voice or Live model reconnects the assistant. The conversation context resets.

## Known limits

- Audio needs a 32-bit float device. Other formats are reported as errors.
- Resampling is linear, not a sinc filter.
- macOS terminal apps need microphone permission. Without it, the mic is silent and voxterm warns you.
