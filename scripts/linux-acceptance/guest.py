"""Execute bounded acceptance probes over a test task's JSON-line stream."""
import json
import subprocess
import sys

for line in sys.stdin:
    try:
        request = json.loads(line)
        if "python" in request:
            scope = {}
            exec(compile(request["python"], "<acceptance>", "exec"), scope)
            result = scope.get("result")
        else:
            child = subprocess.run(request["argv"], capture_output=True, text=True,
                                   timeout=request.get("timeout", 15))
            result = {"rc": child.returncode, "out": child.stdout, "err": child.stderr}
        print(json.dumps({"result": result}), flush=True)
    except Exception as error:
        print(json.dumps({"error": str(error)}), flush=True)
