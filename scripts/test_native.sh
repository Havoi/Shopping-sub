#!/bin/sh
set -eu
cd "$(dirname "$0")/../native"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
cxx=${CXX:-g++}
flags="-std=c++20 -O2 -march=x86-64-v3 -pthread -ffunction-sections -fdata-sections"
# A few helper definitions live in the application translation unit. Give
# its entry point a different name so each test can supply its own main.
"$cxx" $flags -Dmain=shopping_program_main -c main.cpp -o "$work/main.o"
"$cxx" $flags -c links.cpp -o "$work/links.o"
for source in tests/*.cpp; do
    name=$(basename "$source" .cpp)
    "$cxx" $flags "$source" "$work/main.o" "$work/links.o" -Wl,--gc-sections -o "$work/$name"
    "$work/$name"
    echo "Passed $name"
done
