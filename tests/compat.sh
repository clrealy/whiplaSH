#!/bin/sh
# Runs every case in tests/cases.txt through bash and whiplaSH (as script
# files) and checks stdout + exit code match.
#   cargo build --release && sh tests/compat.sh
WS="${WS:-$(pwd)/target/release/whiplash}"
CASES="$(pwd)/tests/cases.txt"
TMP=$(mktemp -d)
pass=0; fail=0; n=0
awk -v dir="$TMP" 'BEGIN{n=1; f=dir"/case1.sh"} /^---$/{n++; f=dir"/case"n".sh"; next} {print > f}' "$CASES"
for c in $(ls "$TMP"/case*.sh | sort -V); do
  n=$((n+1))
  for impl in bash ws; do
    rm -rf "$TMP/run-$impl"; mkdir "$TMP/run-$impl"
    if [ $impl = bash ]; then (cd "$TMP/run-$impl" && bash "$c" > ../out-$impl 2>/dev/null; echo $? > ../rc-$impl)
    else (cd "$TMP/run-$impl" && "$WS" "$c" > ../out-$impl 2>/dev/null; echo $? > ../rc-$impl); fi
  done
  if cmp -s "$TMP/out-bash" "$TMP/out-ws" && cmp -s "$TMP/rc-bash" "$TMP/rc-ws"; then pass=$((pass+1)); else
    fail=$((fail+1)); echo "FAIL case $n:"; sed 's/^/    /' "$c" | head -5
    diff "$TMP/out-bash" "$TMP/out-ws" | head -8; cmp -s "$TMP/rc-bash" "$TMP/rc-ws" || echo "  exit: bash=$(cat $TMP/rc-bash) whiplash=$(cat $TMP/rc-ws)"
  fi
done
rm -rf "$TMP"
echo "passed: $pass  failed: $fail"
[ "$fail" = 0 ]
