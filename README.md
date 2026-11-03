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

## Keys

| Key | Action |
|---|---|
| Enter | Send typed text to the assistant |
| F2 | Mic mute or unmute |
| F3 | Voice picker. `p` previews the highlighted voice |
| F4 | Live model picker |
| F5 | Agent model picker |
| F6 | Toggle mute on blur |
| y / n | Approve or deny a permission request |
| ctrl-c | Cancel the agent. Quits when the agent is idle |
| ctrl-q | Quit |

## Behavior

- **Mute on blur** drops the mic stream when the terminal loses focus. This turns off the macOS mic indicator.
- **Echo gate** stops the mic from sending while the assistant speaks. Barge-in works only with headphones.
- **Voice approvals** cover file edits only. Shell and other actions need a screen confirm.
- **Settings** live in `~/.config/voxterm/config.json`.
- Changing voice or Live model reconnects the assistant. The conversation context resets.

## Known limits

- Audio needs a 32-bit float device. Other formats are reported as errors.
- Resampling is linear, not a sinc filter.
- macOS terminal apps need microphone permission. Without it, the mic is silent and voxterm warns you.
