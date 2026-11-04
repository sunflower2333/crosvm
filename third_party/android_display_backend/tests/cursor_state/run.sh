#!/bin/sh
set -eu
test_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
cursor_test_out=$(mktemp -d /tmp/droidvm-cursor-state.XXXXXX)
trap 'rm -f "$cursor_test_out/cursor_state_test"; rmdir "$cursor_test_out"' EXIT
"${CXX:-c++}" -std=c++17 -O1 -g -Wall -Wextra -Werror -pthread \
    -fsanitize=address,undefined -fno-omit-frame-pointer -I"$test_dir/../.." \
    "$test_dir/cursor_state_test.cpp" -o "$cursor_test_out/cursor_state_test"
"$cursor_test_out/cursor_state_test"
