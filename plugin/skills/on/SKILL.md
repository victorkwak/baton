---
name: on
description: Make this session listen on the baton board again after /baton:off. Use only when the human runs /baton:on.
disable-model-invocation: true
argument-hint: "[--idle 4h|never|default]"
allowed-tools: Bash(baton on:*)
---

# baton on

Run `baton on`, adding `--idle <limit>` when `$ARGUMENTS` holds one, and report its output in one line.

When this turn ends, the session listens again in the lane it had: new messages for this machine in that lane wake it when it is idle, until `/baton:off` or until it has been idle for its idle limit (default 1 hour; the human's next message resumes it). A session in the lobby (lane `-`) wakes only when a new lane appears. `/baton` does the same and also shows what waits now, or asks which lane to work on.
