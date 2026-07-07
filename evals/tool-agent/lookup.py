"""The LIVE lookup tool for `auto run --tool lookup=...` (ADR-0017): the
same function the recorded agent's tool_call implements, spoken over the
pluggable command contract — canonical input JSON as the final argument,
output JSON on stdout."""

import json
import sys

text = json.loads(sys.argv[1])
print(json.dumps(f"team-{text.split()[0][0]}"))
