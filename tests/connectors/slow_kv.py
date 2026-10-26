#!/usr/bin/env python3
"""Test connector: the example kv connector, one second slower per apply, so
a test can drop a request while a sync is still writing."""
import os
import sys
import time

if len(sys.argv) > 1 and sys.argv[1] == "apply":
    time.sleep(1)
here = os.path.dirname(os.path.abspath(__file__))
kv = os.path.join(here, "..", "..", "connectors", "kv.py")
os.execv(sys.executable, [sys.executable, kv] + sys.argv[1:])
