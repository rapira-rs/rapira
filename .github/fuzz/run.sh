#!/bin/sh
# Fuzzes the rapira service with Schemathesis. Then fails when a worker exited or restarted during the session.
# SEED and EXAMPLES select the session. The same values replay it.

status=0

# $1: the target directory, $2: the base URL, $3: the checks.
# The request timeout is above the keepalive and write timeouts of rapira, so a lost response fails the run.
fuzz() {
    SCHEMATHESIS_HOOKS="/fuzz/$1/hooks.py" schemathesis run "/fuzz/$1/openapi.yaml" --url "$2" \
        --seed "$SEED" --max-examples "$EXAMPLES" --phases coverage,fuzzing --workers 1 \
        --generation-database none --request-timeout 90 --checks "$3" --no-color || status=1
}

fuzz http http://rapira:8080 not_a_server_error,status_code_conformance,content_type_conformance,response_schema_conformance,rapira_echo

# The counters of each worker exit and each entrypoint restart. A value other than 0 is a crash.
wget -qO- http://rapira:9180/metrics | awk '
    /^rapira_(worker_exits|script_restarts)_total/ { n++; if ($NF != 0) { print "crash: " $0; bad = 1 } }
    END { exit bad || n == 0 }' || status=1

exit "$status"
