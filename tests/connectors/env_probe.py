#!/usr/bin/env python3
"""Test connector: prints the contract-relevant environment it was given."""
import json
import os
import sys

WATCHED = ["CREDENTIAL_PASSWORD", "APPLICATION_CONFIG", "UNIFIED_ALLOW_WRITE",
           "UNIFIED_GITOPS_TEST_PARENT_SECRET"]

cmd = sys.argv[1] if len(sys.argv) > 1 else ""
if cmd == "list":
    print(json.dumps([{k: os.environ.get(k) for k in WATCHED}]))
elif cmd == "apply":
    json.load(sys.stdin)
    sys.exit(0 if os.environ.get("UNIFIED_ALLOW_WRITE") == "1" else 3)
elif cmd == "describe":
    print(json.dumps({"protocol": 1, "kinds": [{"kind": "probe", "key": "name"}]}))
else:
    sys.exit(2)
