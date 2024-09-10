FROM quay.io/mongodb/bazel-remote-execution:ubuntu22-2024_08_12-19_02_52

SHELL ["/bin/bash", "-o", "pipefail", "-c"]

RUN apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y \
    gcc=4:11.2.0-1ubuntu1 \
    g++=4:11.2.0-1ubuntu1 \
    python3=3.10.6-1~22.04 \
    python3-minimal=3.10.6-1~22.04 \
    libpython3-stdlib=3.10.6-1~22.04 \
    curl=7.81.0-1ubuntu1.17 \
    ca-certificates=20230311ubuntu0.22.04.1 \
    git=1:2.34.1-1ubuntu1.11 \
    unzip=6.0-26ubuntu3.2 \
    openjdk-21-jdk=21.0.4+7-1ubuntu2~22.04 \
    zip=3.0-12build2 \
    npm \
    && apt-get clean \
    && rm -rf /var/lib/apt/lists/*

RUN npm install -g @bazel/bazelisk
RUN git clone https://github.com/tracemachina/mongo.git

RUN cd mongo && \
    cat <<EOF > .bazelrc.local
--config=nativelink
# Remote execution / Cache settings
build:nativelink --remote_executor=scheduler-mongo-ci-dev.dev-usw2.nativelink.net:443
build:nativelink --remote_cache=cas-mongo-ci-dev.dev-usw2.nativelink.net:443
build:nativelink --remote_header=x-nativelink-api-key=3cb3f207802c18cc19778e83b8cd6ab8478b8517191dfcf2c4090fea443f32bf
# TODO(adams): paramterize over this value after reverting the execution properties changes.
build:nativelink --remote_default_exec_properties="container-image=docker://ubuntu22:latest"
build:nativelink --remote_instance_name=main
build:nativelink --remote_download_minimal
build:nativelink --remote_timeout=600
# BES settings
build:nativelink --bes_backend=grpcs://bes-mongo-ci-dev.dev-usw2.nativelink.net
build:nativelink --bes_header=x-nativelink-api-key=3cb3f207802c18cc19778e83b8cd6ab8478b8517191dfcf2c4090fea443f32bf
build:nativelink --bes_results_url=https://dev.nativelink.com/a/tracemachina/build
# Nativelink does not support, disabled for now.
build:nativelink --experimental_remote_cache_compression=false
# Number of bazel jobs to run
build:nativelink --jobs 500
# Debug logging settings
# build:nativelink --remote_grpc_log=/tmp/grpc.log
# build:nativelink --execution_log_json_file=/tmp/execution.log
# build:nativelink --build_event_json_file=/tmp/build_events.json
# build:nativelink --verbose_failures
# build:nativelink -s
# build:nativelink --platform_suffix=c2
EOF

RUN cd mongo && bazel fetch //src/... -k || true
RUN cd mongo && \
    bazel shutdown

RUN cat <<EOF > ~/run.sh
#!/bin/bash

set -x

cd mongo

while true ; do
  # Clean up any changes from the previous iteration.
  git reset --hard

  # Find all .h files in the current directory and its subdirectories
  files=($(find . -type f -name "*.h"))

  # Check if any .h files were found
  if [ ${#files[@]} -eq 0 ]; then
    echo "No .h files found."
    exit 1
  fi

  # Randomly select one of the .h files
  random_file="${files[$RANDOM % ${#files[@]}]}"

  # Count the number of lines in the selected file
  num_lines=$(wc -l < "$random_file")

  # Check if the file has at least one line
  if [ "$num_lines" -eq 0 ]; then
    echo "Selected file has no lines: $random_file"
    exit 1
  fi

  # Randomly decide how many new lines to inject (between 1 and 5)
  new_lines=$((RANDOM % 5 + 1))

  # Loop to inject random new lines
  for ((i=0; i<$new_lines; i++)); do
    # Randomly select a line number in the file
    line_num=$((RANDOM % num_lines + 1))
    
    # Insert a new line at the selected line number
    sed -i "${line_num}i\\" "$random_file"
  done

  echo "Inserted $new_lines new line(s) into file: $random_file"

  # Run bazel command.
  bazel build //src/... --config=nativelink --remote_download_toplevel -k
done
EOF

RUN chmod +x ~/run.sh

ENTRYPOINT [ "bash", "~/run.sh" ]