#!/bin/bash

set -xuo pipefail
COUNT=$1  # Maximum number of worker jobs at any time
shift

export PUBLIC_PORT=50051
cargo run --profile=smol -- $PWD/nativelink-config/examples/redis_scheduler_cas.json &

cd ~/nativelink

# Function to randomly stop a running worker job
function stop_worker() {
    if [ ${#WORKER_PIDS[@]} -gt 0 ]; then
        local idx=$(( RANDOM % ${#WORKER_PIDS[@]} ))
        PID="${WORKER_PIDS[$idx]}"
        PORT_NUM="${PORT_NUMS[$idx]}"
        kill -9 "$PID" || true
        wait "$PID" || true
        sudo rm -rf "/tmp/nativelink/work$PORT_NUM" "/tmp/nativelink/data-worker-test-$PORT_NUM"
        unset WORKER_PIDS[$idx]
        WORKER_PIDS=("${WORKER_PIDS[@]}")
        unset PORT_NUMS[$idx]
        PORT_NUMS=("${PORT_NUMS[@]}")
    fi
}

# Array to store worker PIDs
declare -a WORKER_PIDS=()
declare -a PORT_NUMS=()

set -x
# Main loop to randomly create and destroy worker jobs
while true; do
    # Randomly decide to start a new worker job
    if [ "${#WORKER_PIDS[@]}" -lt "$COUNT" ] && [ $(( RANDOM % 10 )) -gt 2 ]; then
        export WORKER_ENDPOINT_PORT=$(( 50100 + RANDOM % 300 ))
        cargo run --profile=smol -- $PWD/nativelink-config/examples/redis_worker.json &
        PID=$!
        WORKER_PIDS+=( $PID )
        PORT_NUMS+=( $WORKER_ENDPOINT_PORT )
    fi

    # Randomly decide to stop an existing worker job
    if [ "${#WORKER_PIDS[@]}" -gt 0 ] && [ $(( RANDOM % 10 )) -gt 5 ]; then
        stop_worker
    fi

    # Sleep for a random interval between 1 and 5 seconds
    sleep $(( RANDOM % 5 + 1 ))
done

wait