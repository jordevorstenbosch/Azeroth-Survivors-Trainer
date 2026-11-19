# Azeroth Survivors Trainer

A standalone Windows x64 trainer for the original Azeroth Survivors fan game. Version 3.0.0 is written in Rust and runs from a single executable.

## Get started

1. Download `AzerothSurvivorsTrainer.exe` from the [latest release](https://github.com/nelbin4/Azeroth-Survivors-Trainer/releases/latest).
2. Open the trainer. On first use, choose **Locate game** and select `AzerothSurvivors.exe`.
3. Start a solo run or a game you host, then open the character panel with **C**.
4. Press **Start watcher** if the watcher is stopped. Use **Stop watcher** or close the trainer to stop rewriting stats.

Use the trainer in solo runs or games you host. Version 3.0.0 gameplay testing covered solo runs.

## Supported stats

| Stat | Target |
| --- | ---: |
| Damage | +9999% |
| Area | +9999% |
| Luck | +9999% |
| Gold Bonus | +9999% |

The watcher matches the character panel to the live player record. It starts rewriting the first verified stat every 0.5 seconds and adds the others as they are verified. It keeps using the same record through level-ups. The game's recalculation can briefly reset a value before the next rewrite.

## Updates and troubleshooting

- **Check updates** checks GitHub. When a newer Rust release is available, **Download & Install** verifies and installs the executable and restarts the trainer.
- **Finding your character** means the watcher is waiting for the character panel. Keep it open until the trainer shows **Stats frozen**.
- **Unsupported game build** means this release cannot freeze stats on that executable. The verified Windows game build starts with SHA-256 `ba09f96a327c`.
- Settings and `trainer.log` are stored under `%LOCALAPPDATA%\Azeroth Survivors Trainer`.

The [Python v2.1.3 release](https://github.com/nelbin4/Azeroth-Survivors-Trainer/releases/tag/v2.1.3) remains available for users of the legacy trainer.

## Development

The [Build Rust trainer workflow](.github/workflows/build-rust-trainer.yml) runs the Rust tests and builds the Windows x64 executable. Each release uses the successful workflow artifact from its release commit.

This is an unofficial fan tool. See the [MIT license](LICENSE).
