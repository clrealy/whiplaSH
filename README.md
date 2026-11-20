<h1 align="center">whiplaSH 🥁</h1>

<p align="center"><b>Runs your bash scripts. Feels like fish.</b><br>Part of <a href="https://github.com/clrealy/RNG">RNG's Not GNU!!!</a></p>

---

whiplaSH is a shell written in Rust that speaks **bash** (so your scripts just work)
with a **fish**-style interactive side: autosuggestions, syntax highlighting and a
clean prompt. You can mix both syntaxes freely.

## Fish vibes

- **Autosuggestions** from your history, shown in gray. Press → to accept.
- **Syntax highlighting:** real commands are green, typos red, keywords blue, strings yellow.
- **Tab completion** for commands, files and `$VARIABLES`.
- **fish-style prompt:** `~/P/rng ❯`, which turns red and shows the code when a command fails.
- Multi-line editing: unfinished `if`/`for`/quotes keep the prompt open.
- `~/.whiplashrc` runs at startup, history lives in `~/.whiplash_history`.
- Set `whiplash_greeting` to change the greeting (empty = none), or `PS1` for a bash-style prompt.

## Both syntaxes work

```sh
# bash                                  # fish
if [ -f x ]; then echo yes; fi          if test -f x; echo yes; end
for f in *.rs; do wc -l "$f"; done      for f in *.rs; wc -l $f; end
greet() { echo "hi $1"; }               function greet; echo "hi $argv"; end
export EDITOR=nah                       set -x EDITOR nah
name=world                              set name world
cmd && echo ok || echo no               cmd; and echo ok; or echo no
! grep -q x f                           not grep -q x f
echo "$(pwd)"                           echo (pwd)
{ a; b; } > out                         begin; a; b; end > out
```

## Bash support

Pipelines, `&&` `||` `;` `&`, `!`, subshells `( )`, groups `{ }`, `if/elif/else`,
`while`/`until`, `for`, `case`, functions (`f()`, `function f`, `f() ( … )`),
`local`, `return`, `break N`/`continue N`.

Expansions: `$var` `${var}` `${#var}` `${var:-x}` `${var:=x}` `${var:+x}` `${var:?x}`
`${var#p}` `${var##p}` `${var%p}` `${var%%p}` `${var/p/r}` `${var//p/r}` `${var:off:len}`
`${var^^}` `${var,,}`, `$(…)` and backticks, `$((…))` (full integer math), `~`,
brace expansion `{a,b}` `{1..10}` `{01..05}`, globs `* ? [a-z] [[:alpha:]]`,
word splitting with `IFS`, `"$@"`, `$'…'` strings.

Redirections: `<` `>` `>>` `<>` `2>&1` `>&-` `&>` `&>>` `|&`, heredocs `<<EOF`
`<<'EOF'` `<<-EOF`, here-strings `<<<`, `exec 3>file`.

Builtins: `cd pwd echo read test [ [[ ]] export unset set shift source . eval exec
exit return break continue local declare alias unalias type command builtin let
getopts wait jobs umask history help` + fish's `and or not functions`.

Options: `set -e`, `set -u`, `set -x`, `set -f`, `set -o pipefail` (also as command-line flags).

## Build & run

```sh
cargo build --release
./target/release/whiplash               # interactive
./target/release/whiplash script.sh     # run a script
./target/release/whiplash -c 'echo hi'  # one command
```

## Testing

```sh
cargo test
sh tests/compat.sh   # 103 scripts run through bash AND whiplaSH; output + exit codes must match
sh tests/fish.sh     # fish-style syntax
```

It also runs real system scripts like `ldd` and `which` unchanged.

## Not yet

Arrays, `=~` regex in `[[ ]]`, job control (`fg`/`bg`/Ctrl-Z), `trap`, `printf` as a
builtin (the external one is used), process substitution `<( )`, `select`.

## Things to know

- `set NAME value` is fish-style (sets a variable). For bash's "set positional args", use
  `set -- a b c`.
- Aliases only expand in interactive mode, like bash.

## License

[RNG Roller License (RRL) v1.0](LICENSE).
