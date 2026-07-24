#!/bin/sh
# POSIX sh conformance probe — the exact failure classes deno_task_shell breaks on.
# Run under any candidate shell: $1 = label
run() { # run <name> <body>
  out=$( { eval "$2"; } 2>&1 ); rc=$?
  printf '%-26s rc=%-3s %s\n' "$1" "$rc" "$(printf '%s' "$out" | head -2 | tr '\n' '|')"
}
echo "### shell: $LBL"
run "braced-simple"      'V=hi; echo "[${V}]"'
run "braced-default"     'echo "[${NOPE:-4096}]"'
run "braced-assign"      'echo "[${NP2:=x}][$NP2]"'
run "braced-alt"         'V=1; echo "[${V:+set}]"'
run "braced-prefix-strip" 'V=abcdef; echo "[${V#abc}]"'
run "braced-suffix-strip" 'V=abcdef; echo "[${V%def}]"'
run "braced-length"      'V=abc; echo "[${#V}]"'
run "dollar-zero"        'echo "[$0]"'
run "arith"              'echo "[$((3000+1))]"'
run "for-loop"           'for i in a b; do printf %s "$i"; done; echo'
run "while-loop"         'i=0; while [ $i -lt 2 ]; do i=$((i+1)); done; echo "[$i]"'
run "until-loop"         'i=0; until [ $i -ge 2 ]; do i=$((i+1)); done; echo "[$i]"'
run "if-else"            'if [ 1 -eq 1 ]; then echo yes; else echo no; fi'
run "case"               'case abc in a*) echo matched;; *) echo no;; esac'
run "function-def"       'f() { echo infunc; }; f'
run "multi-redirect"     'echo hi >/tmp/_pp1 2>&1; cat /tmp/_pp1'
run "redirect-two-files" 'echo hi >/tmp/_pp2 2>/tmp/_pp3; cat /tmp/_pp2'
run "heredoc"            'cat <<EOF
hd-ok
EOF'
run "trap-exit"          '(trap "echo trapped" EXIT; true)'
run "bg-job-then-exit"   'sleep 5 & echo started; kill %1 2>/dev/null'
run "stderr-pipe"        'sh -c "echo E 1>&2; echo O" 2>&1 | cat'
run "apostrophe-arg"     "echo -- 'it'\\''s'"
run "test-bracket"       '[ 1 = 1 ] && echo brackok'
run "test-a-compose"     '[ 1 = 1 -a 2 = 2 ] && echo composeok'
run "echo-n"             'echo -n abc; echo'
run "positional"         'set -- p1 p2; echo "[$1][$2][$#]"'
run "brace-group"        '{ echo grp; }'
run "subshell"           '(echo sub)'
run "cmdsub"             'echo "[$(echo cs)]"'
run "exit-status"        'false; echo "[$?]"'
run "source-dot"         'echo "echo sourced" >/tmp/_pps; . /tmp/_pps'
run "shift"              'set -- a b c; shift; echo "[$1]"'
run "local-var"          'f(){ local x=1; echo "[$x]"; }; f'
run "IFS-split"          'IFS=,; set -- $(echo "a,b"); echo "[$1][$2]"'
