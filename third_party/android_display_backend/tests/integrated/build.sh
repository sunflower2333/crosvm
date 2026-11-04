#!/bin/bash
set -euo pipefail
workspace=/home/sunf/droidvm-repos
export PWF_PLAN_ROOT="$workspace/.planning/native-display-shared-allocation-20260912"
node "$(dirname -- "$0")/prepare-build.mjs" --refresh-sources
cd "$workspace/.artifacts/native-shared-allocation-aosp"
export PATH="$workspace/.artifacts/large-backing-tools/root/usr/bin:$PATH"
export TARGET_PRODUCT=aosp_arm64 TARGET_RELEASE=trunk_staging TARGET_BUILD_VARIANT=eng
export ALLOW_MISSING_DEPENDENCIES=true TARGET_BUILD_UNBUNDLED=true
export ALLOW_BP_UNDER_SYMLINKS=false OUT_DIR=out
build/soong/soong_ui.bash --make-mode "${@:-crosvm_device_only}" -j12
echo NATIVE_SHARED_ALLOCATION_MATCHED_BUILD_PASS
