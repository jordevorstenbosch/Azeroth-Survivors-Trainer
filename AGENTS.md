# Building the trainer

Use the **Build Rust trainer** GitHub Actions workflow for Windows executable
builds. Push build inputs to trigger the workflow and retrieve the executable
from the workflow run's artifacts. Use a successful Actions artifact when
packaging a Rust trainer release.

# Trainer watcher behavior

Every watcher entry point must discover the local player's verified stat
record, immediately apply the configured maximum targets, and keep those
targets frozen. Discovery is a step in this workflow. Do not offer a
discovery-only or read-only watcher mode.

The supported targets are Damage, Area, Gold Bonus, and Luck. Once the
watcher binds the live record, rewrite all four every 0.5 seconds. Keep
using that record through level-ups. If the record cannot be verified or
is lost, show what the watcher is waiting for and retry discovery.
