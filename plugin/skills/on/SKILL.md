---
name: on
description: Make this session listen on the baton board again after /baton:off. Use only when the human runs /baton:on.
disable-model-invocation: true
allowed-tools: Bash(baton on:*)
---

# baton on

Run `baton on` and report its output in one line.

When this turn ends, the session listens again in the lane it had: new messages for this machine in that lane wake it when it is idle, until `/baton:off`. A session in the lobby (lane `-`) wakes only when a new lane appears. `/baton` does the same and also shows what waits now, or asks which lane to work on.
