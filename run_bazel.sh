#!/bin/bash

set -xuo pipefail
MAX_JOBS=$1  # Maximum number of Bazel invocations at any time
shift

# Initialize the platform_suffix as a number
PLATFORM_SUFFIX=1
SUFFIX_INCREMENT_INTERVAL=1  # Time interval in seconds to increment the platform_suffix
LAST_INCREMENT_TIME=1

# Function to start a new Bazel job
start_bazel_job() {
    local i=$(( RANDOM % 10000 ))  # Generate a random number for i

    sudo rm -rf /tmp/bazel-$i
    timeout 20 cp -rf /tmp/bazel /tmp/bazel-$i \
        && bazel --output_base=/tmp/bazel-$i build //... --config=self_test --config=self_execute \
          --remote_cache=grpc://127.0.0.1:50051 --remote_executor=grpc://127.0.0.1:50051 \
          --platform_suffix=$PLATFORM_SUFFIX --remote_download_minimal -j 200
    sudo rm -rf /tmp/bazel-$i
    # local BAZEL_PID=$!

    # # Start a watcher in the background to ensure cleanup
    # (
    #     tail --pid=$BAZEL_PID -f /dev/null || true  # Wait for the Bazel test job to finish or be killed
    #     bazel --output_base=/tmp/bazel-$i shutdown || true
    #     sudo rm -rf /tmp/bazel-$i
    # ) >/dev/null 2>&1 &

    # echo $BAZEL_PID
}

# Function to randomly stop a running Bazel job
stop_bazel_job() {
    if [ ${#BAZEL_PIDS[@]} -gt 0 ]; then
        local idx=$(( RANDOM % ${#BAZEL_PIDS[@]} ))
        kill -9 "${BAZEL_PIDS[$idx]}"
        BAZEL_PIDS=("${BAZEL_PIDS[@]:0:$idx}" "${BAZEL_PIDS[@]:$((idx + 1))}")
    fi
}

# Array to store Bazel PIDs
declare -a BAZEL_PIDS=()

# Main loop to manage Bazel jobs
while true; do
    CURRENT_TIME=$(date +%s)

    # Increment platform_suffix based on time interval
    if (( CURRENT_TIME - LAST_INCREMENT_TIME >= SUFFIX_INCREMENT_INTERVAL )); then
        PLATFORM_SUFFIX=$((PLATFORM_SUFFIX + 1))
        LAST_INCREMENT_TIME=$CURRENT_TIME
        start_bazel_job
    fi

    # # Randomly decide to start a new Bazel job
    # if [ "${#BAZEL_PIDS[@]}" -lt "$MAX_JOBS" ] && [ $(( RANDOM % 10 )) -gt 8 ]; then
    #     start_bazel_job &
    #     BAZEL_PIDS+=( $! )
    # fi

    # # Randomly decide to stop an existing Bazel job
    # if [ "${#BAZEL_PIDS[@]}" -gt 0 ] && [ $(( RANDOM % 10 )) -gt 8 ]; then
    #     stop_bazel_job
    # fi

    # Sleep for a random interval between 1 and 30 seconds
    # sleep $(( RANDOM % 30 + 1 ))
    sleep .2
done

wait
