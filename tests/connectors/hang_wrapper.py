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
with open(config["pidfile"], "w") as f:
    f.write(str(child.pid))
child.wait()
