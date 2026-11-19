# Building the trainer

Use the **Build Rust trainer** GitHub Actions workflow for Windows executable
builds. Push build inputs to trigger the workflow and retrieve the executable
from the workflow run's artifacts. Use a successful Actions artifact when
packaging a Rust trainer release.

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
