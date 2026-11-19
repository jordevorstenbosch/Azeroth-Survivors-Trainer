# Building the trainer

GitHub Actions always builds the Windows trainer, including the latest
release. Use the **Build Rust trainer** workflow for every Windows build.
For a release, push the release commit to `main` and wait for that commit's
workflow run to pass. Publish its `AzerothSurvivorsTrainer.exe` artifact as
the Windows release asset.

# Trainer watcher behavior

The trainer's purpose is to discover, max, and freeze the local player's
supported stats as soon as the live record is verified. Every watcher
entry point follows this workflow. Do not offer a discovery-only or
read-only watcher mode.

Discover once per live record and apply the maximum targets immediately.
Here, freeze means polling and unconditionally rewriting the maximum
values for Damage, Area, Gold Bonus, and Luck every 0.5 seconds, without
checking whether individual values reset. Keep using the bound record
through level-ups. If no record can be verified or the record is lost,
show what the watcher is waiting for and retry discovery.
