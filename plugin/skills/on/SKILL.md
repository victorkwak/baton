---
name: on
description: Make this session listen on the baton board again after /baton:off. Use only when the human runs /baton:on.
disable-model-invocation: true
allowed-tools: Bash(baton on:*)
---

# baton on

Run `baton on` and report its output in one line.

When this turn ends, the session listens: any new message for this machine (new plans too) wakes it when it is idle, until `/baton:off`. `/baton` does the same and also shows what waits now.
