#!/bin/sh
# Spike apiKeyHelper: a dummy, invalid key (no real key exists). Logs each invocation.
date +%s >> <spike>/markers/helper.log
# The recorded run printed a dummy, invalid key-shaped string; redacted so none is committed.
echo sk-ant-<redacted>
