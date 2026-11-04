#!/bin/sh
# Host test for SurfaceBufferPresenter's apply ordering, with a SurfaceControl double.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
out=$(mktemp -d)
trap 'rm -rf "$out"' EXIT
# The presenter includes its siblings with quotes: stage it beside the double.
cp "$here/../../surface_buffer_presenter.h" "$here/../../consumer_release.h" \
    "$here/../../native_fifo_trace.h" \
    "$here/../../scanout_presentation_geometry.h" "$here/../../scanout_geometry_abi.h" \
    "$here/../../display_timing.h" "$here/surface_control_dl.h" "$out/"
c++ -std=c++17 -O1 -Wall -Wextra -Werror -pthread -I"$out" -I"$here/stub" \
    "$here/presenter_async_test.cpp" -o "$out/presenter_async_test"
"$out/presenter_async_test"
c++ -std=c++17 -O1 -g -Wall -Wextra -Werror -pthread \
    -fsanitize=address,undefined -fno-omit-frame-pointer -I"$out" -I"$here/stub" \
    "$here/presenter_lifetime_test.cpp" -o "$out/presenter_lifetime_test"
# The production monitor and executor intentionally live until process exit.
ASAN_OPTIONS=detect_leaks=0 "$out/presenter_lifetime_test"
c++ -std=c++17 -O1 -g -Wall -Wextra -Werror -pthread \
    -fsanitize=address,undefined -fno-omit-frame-pointer -I"$out" -I"$here/stub" \
    "$here/presenter_frame_rate_test.cpp" -o "$out/presenter_frame_rate_test"
ASAN_OPTIONS=detect_leaks=0 "$out/presenter_frame_rate_test"
c++ -std=c++17 -O1 -g -Wall -Wextra -Werror -pthread \
    -fsanitize=address,undefined -fno-omit-frame-pointer -I"$out" -I"$here/stub" \
    "$here/presenter_geometry_test.cpp" -o "$out/presenter_geometry_test"
ASAN_OPTIONS=detect_leaks=0 "$out/presenter_geometry_test"
c++ -std=c++17 -O1 -g -Wall -Wextra -Werror -pthread \
    -fsanitize=address,undefined -fno-omit-frame-pointer -I"$out" -I"$here/stub" \
    "$here/presenter_timing_test.cpp" -o "$out/presenter_timing_test"
ASAN_OPTIONS=detect_leaks=0 "$out/presenter_timing_test"
