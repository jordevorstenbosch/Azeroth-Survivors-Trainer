# Azeroth Survivors Trainer

A standalone Rust trainer for the original Azeroth Survivors fan game on Windows x64.

## Get started

1. Download `AzerothSurvivorsTrainer.exe` from the [latest release](https://github.com/nelbin4/Azeroth-Survivors-Trainer/releases/latest).
2. Open the trainer. On first use, choose **Locate game** and select `AzerothSurvivors.exe`.
3. Start a solo run or a game you host. With **OCR** detection, open the character panel with **C**.
4. Press **Start watcher** if the watcher is stopped. Use **Stop watcher** or close the trainer to stop rewriting stats.

Use the trainer in solo runs or games you host. Gameplay testing covered solo runs.

In **Settings → Detection Mode**, choose **OCR** to use the character panel or
**Read Memory** to match the game's native stat messages against the live player
record. In Read Memory, gain experience in a solo run so the watcher can
distinguish the active record from stale copies.

## Supported stats

| Stat | Target |
| --- | ---: |
| Damage | +9999% |
| Area | +9999% |
| Luck | +9999% |
| Gold Bonus | +9999% |

The watcher starts rewriting verified stats every 0.5 seconds and keeps using the same record through level-ups. The game's recalculation can briefly reset a value before the next rewrite.

## Updates and troubleshooting

- **Check updates** checks GitHub. When a newer Rust release is available, **Download & Install** verifies and installs the executable and restarts the trainer.
- **Finding your character** means the watcher is waiting for the character panel. Keep it open until the trainer shows **Stats frozen**.
- **Unsupported game build** means this release cannot freeze stats on that executable. The verified Windows game build starts with SHA-256 `ba09f96a327c`.
- Settings and `trainer.log` are stored under `%LOCALAPPDATA%\Azeroth Survivors Trainer`.

The [Python v2.1.3 release](https://github.com/nelbin4/Azeroth-Survivors-Trainer/releases/tag/v2.1.3) remains available for users of the legacy trainer.

## Development

The [Build Rust trainer workflow](.github/workflows/build-rust-trainer.yml) runs the Rust tests and builds the Windows x64 executable. Each release uses the successful workflow artifact from its release commit.

### Project layout

| Path | Contents |
| --- | --- |
| `src/` | Rust GUI, game discovery, stat watcher, settings, and updater |
| `assets/` | Trainer icons |
| `docs/legacy/` | [Historical trainer research](docs/legacy/TECHNICAL_NOTES.md) |
| `releases/` | Local Windows executable and archived builds |
| `local/` | Local tools, research, and legacy data |

Cargo reads `Cargo.toml`, `Cargo.lock`, and `build.rs` at the repository root. Git ignores `releases/` and `local/`.

This is an unofficial fan tool. See the [MIT license](LICENSE).
