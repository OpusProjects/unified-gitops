#!/usr/bin/env python3
"""Test connector: a wrapper that starts the real (hung) work as a child
without exec, and writes the child's pid where APPLICATION_CONFIG says, to
prove a timeout kills the child too."""
import json
import os
import subprocess
import sys

config = json.loads(os.environ.get("APPLICATION_CONFIG", "{}"))
child = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(3600)"])
# Written whole or not at all: an empty pidfile read mid-write would name no
# process, and the test would call that "killed".
tmp = config["pidfile"] + ".tmp"
with open(tmp, "w") as f:
    f.write(str(child.pid))
os.replace(tmp, config["pidfile"])
child.wait()
