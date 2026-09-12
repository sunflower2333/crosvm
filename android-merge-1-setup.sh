#!/bin/bash

set -ex

function usage() { echo "$0 [-s][-b] <bug number>" && exit 1; }

sync=""
branch=""
while getopts 'sb' FLAG; do
  case ${FLAG} in
    s)
      sync="sync"
      ;;
    b)
      branch="branch"
      ;;
    ?)
      echo "unknown flag."
      usage
      ;;
  esac
done

shift $((OPTIND-1))
if [ $# != 1 ]; then
    echo "Requires exactly 1 positional argument (bug number)."
    usage
fi
bug_number="$1"

if [ "$sync" = "sync" ]
then
  read -p "This script will sync your crosvm project. Do you wish to proceed? [y/N]" -n 1 -r
  if [[ ! $REPLY =~ ^[Yy]$ ]]
  then
    exit 1;
  fi
fi

if [ -z $ANDROID_BUILD_TOP ]; then echo "forgot to source build/envsetup.sh?" && exit 1; fi
cd $ANDROID_BUILD_TOP/external/crosvm

if [[ ! -z $(git branch --list merge) && ! "$branch" = "branch" ]];
  then
    echo "branch merge already exists. Forgot to clean up?" && exit 1;
fi

# needed in 'install-deps', but timeout is too tight still sometimes
sudo echo Sudo prepared.

rustup update

# TODO: sometimes we want to sync the entire tree, and sometimes we only
# want to fetch upstream. Should we have independent options?
if [ "$sync" = "sync" ]
then
  repo sync -c -j96
  git fetch aosp upstream-main
fi

source $ANDROID_BUILD_TOP/build/envsetup.sh
m blueprint_tools cargo_embargo crosvm

if [ ! "$branch" = "branch" ];
  then
    repo start merge;
fi

git merge --log aosp/upstream-main --no-edit
OLD_MSG=$(git log --format=%B -n1)
git commit --amend -m "$OLD_MSG
Bug: $bug_number
Test: TH"

$ANDROID_BUILD_TOP/external/crosvm/tools/deps/install-x86_64-other
$ANDROID_BUILD_TOP/external/crosvm/android-fork-stats.sh

# continue if the merge was clean
./android-merge-2-cargo-embargo.sh

git commit --amend -a --no-edit

# TODO: add more automated local tests/run host tests?
m crosvm

repo upload . $(cat OWNERS.android | grep @google | sed 's/^/--re=/')
