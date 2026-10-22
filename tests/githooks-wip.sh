#!/bin/sh
# .githooks/pre-push (#492): a push that leaves main's head on a WIP commit is
# refused; a finished head (WIP commits inside it included), a WIP head on a
# wip/ branch and the override go through. A throwaway repo and a bare remote.
set -eu
HERE=$(cd "$(dirname "$0")" && pwd)
HOOK="$HERE/../.githooks/pre-push"
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
git init -q --bare "$WORK/remote.git"
git init -q -b main "$WORK/repo"
cd "$WORK/repo"
git config user.email t@example.invalid
git config user.name t
git config core.hooksPath "$(dirname "$HOOK")"
git remote add origin "$WORK/remote.git"
fail=0
check() { if [ "$2" = "$3" ]; then echo "  ok    $1"; else echo "  FAIL  $1: expected $2, got $3"; fail=1; fi; }
c() { echo "$1" >> f; git add f; git commit -q -m "$1"; }
push() { if git push -q "$@" >/dev/null 2>&1; then echo pushed; else echo refused; fi; }

echo "pre-push (#492):"
c "feat: one"
check "a finished head goes to main" pushed "$(push origin main)"
c "WIP(#1): half done"
check "a WIP head is refused on main" refused "$(push origin main)"
check "it goes to a wip/ branch" pushed "$(push origin HEAD:wip/1-half)"
check "lower-case wip is refused too" refused "$(git commit -q --amend -m 'wip: half' && push origin main)"
check "the override pushes it" pushed "$(STORMBLOCK_ALLOW_WIP_MAIN=1 push origin main)"
c "fix(#1): done"
check "a finished head with WIP commits in it goes" pushed "$(push origin main)"
[ "$fail" = 0 ] && echo "all pre-push checks passed" || { echo FAILURES; exit 1; }
