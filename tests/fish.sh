#!/bin/sh
# fish-style syntax: whiplaSH-only, checked against expected output.
WS="${WS:-$(pwd)/target/release/whiplash}"
pass=0; fail=0
t() { # $1 = script, $2 = expected output
  got=$("$WS" -c "$1" 2>&1)
  if [ "$got" = "$2" ]; then pass=$((pass+1)); else fail=$((fail+1)); printf 'FAIL: %s\n  want: %s\n  got:  %s\n' "$1" "$2" "$got"; fi
}
t 'if test 1 -eq 1; echo yes; else; echo no; end' 'yes'
t 'if test 1 -eq 2; echo a; else if test 2 -eq 2; echo b; else; echo c; end' 'b'
t 'for x in a b c; echo $x; end' 'a
b
c'
t 'set i 0; while test $i -lt 3; echo $i; set i $((i + 1)); end' '0
1
2'
t 'function hi; echo "hi $argv"; end; hi there' 'hi there'
t 'begin; echo one; echo two; end | wc -l' '2'
t 'set name world; echo "hello $name"' 'hello world'
t 'set -x GREET yo; sh -c "echo \$GREET"' 'yo'
t 'set fruits apple banana; echo $fruits' 'apple banana'
t 'set -e fruits; set fruits x; set -e fruits; echo "[$fruits]"' '[]'
t 'set -q HOME; echo $?; set -q NOPE_NOT_SET; echo $?' '0
1'
t 'false; and echo no; or echo yes' 'yes'
t 'true; and echo yes; or echo no' 'yes'
t 'not false; echo $?' '0'
t 'echo (echo fish subst)' 'fish subst'
t 'echo (printf "a\nb")' 'a b'
t 'case x in x) echo matched; end' 'matched'
t 'while false; echo never; end; echo after' 'after'
t 'if true; then echo mixed; end' 'mixed'
t 'for i in 1 2; do echo $i; end' '1
2'
t 'echo {1..3} {a,b}x' '1 2 3 ax bx'
echo "passed: $pass  failed: $fail"
[ "$fail" = 0 ]
