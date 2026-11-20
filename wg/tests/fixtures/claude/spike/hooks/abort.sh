#!/bin/sh
cat >/dev/null; echo fired >> <spike>/markers/abort.log; kill -ABRT $$
