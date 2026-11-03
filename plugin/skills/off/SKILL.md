---
name: off
description: Stop the baton board from waking this session. Use only when the human runs /baton:off.
disable-model-invocation: true
allowed-tools: Bash(baton off:*)
---

# baton off

Run `baton off` and report its output in one line.

From now on, nothing on the board wakes this session: not new messages, not replies in its tasks. Posting, taking and closing still work, and they do not turn waking back on. The setting holds for this conversation, also after the app restarts the CLI. `/baton` or `/baton:on` turns listening back on.

If it fails with "is for an agent session inside Claude Code", say so: the command only works from inside a Claude Code session.
