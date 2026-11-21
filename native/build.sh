#!/bin/sh
set -eu
native_output=${1:-shopping-sessions}
wide_output=${2:-"${native_output}-v4"}
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
output="$work/engine"
arch_flags="-march=x86-64-v3 -mtune=generic"
compile() {
  ${CXX:-g++} -std=c++20 -O3 -DNDEBUG $arch_flags -static-libstdc++ -static-libgcc -Wa,-mbranches-within-32B-boundaries -flto=auto -pthread "$@" \
    main.cpp links.cpp ingest.cpp aggregate.cpp output.cpp -l:liblz4.so.1 -o "$output"
}
cat >"$work/cpu.c" <<'C'
int main(void) {
    return __builtin_cpu_supports("x86-64-v4") ? 4 : __builtin_cpu_supports("x86-64-v3") ? 3 : 0;
}
C
cc "$work/cpu.c" -o "$work/cpu"
level=0
"$work/cpu" || level=$?
if [ "$level" -ge 3 ]; then
  cat >"$work/dump.c" <<'C'
extern void __gcov_dump(void);
extern void __real__exit(int);
void __wrap__exit(int status) { __gcov_dump(); __real__exit(status); }
C
  cc -c "$work/dump.c" -o "$work/dump.o"
  python3 training.py "$work/input"
fi
train() {
  profile_dir=$1
  train_output=$2
  compile "-fprofile-generate=$profile_dir" "$work/dump.o" -Wl,--wrap=_exit
  NCPU=4 "$output" "$work/input" "$train_output"
}
if [ "$level" -ge 3 ]; then
  echo "Building trained x86-64-v3 engine"
  train "$work/counters-v3" "$work/out-v3"
  compile "-fprofile-use=$work/counters-v3" -fprofile-correction -Werror=missing-profile
else
  compile
fi
install -m755 "$output" "$native_output"
arch_flags="-march=x86-64-v4 -mtune=generic"
if [ "$level" -ge 4 ]; then
  echo "Building x86-64-v4 engine with native training"
  train "$work/counters-v4" "$work/out-v4"
  compile "-fprofile-use=$work/counters-v4" -fprofile-correction -Werror=missing-profile
elif [ "$level" -eq 3 ]; then
  echo "Building x86-64-v4 engine with compatible x86-64-v3 profiles"
  # GCC checks each profile's control-flow checksum. Reuse matching counts
  # and compile new or structurally different v4 functions without a profile.
  compile "-fprofile-use=$work/counters-v3" -fprofile-correction -fprofile-partial-training \
    -Wno-error=coverage-mismatch -Wno-missing-profile
else
  compile
fi
install -m755 "$output" "$wide_output"