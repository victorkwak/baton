---
name: on
description: Let the baton waiter wake this session again after /baton:off. Use only when the human runs /baton:on.
disable-model-invocation: true
allowed-tools: Bash(baton on:*)
---

# baton on

Run `baton on` and report its output in one line.

Waking resumes when this turn ends. A reply that arrived while waking was off wakes the session then, if it is still open.
