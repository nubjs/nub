#!/usr/bin/env bash
# Statusline: glanceable LIST of decisions awaiting the maintainer.
# DERIVES the list from fray threads (no static store): scans .fray/*.md and
# selects threads with `status: needs-decision`, printing one row per thread —
# " • [<slug>] <first ~120 chars of statusText>". Claude Code statuslines render
# each printed line as a separate row. Pure file scan, no network — fast.
# stdin JSON is ignored. The rich full-statusText view is scripts/fray/decisions.mjs.
set -euo pipefail

dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# Drain stdin (Claude pipes session JSON in) without blocking.
cat >/dev/null 2>&1 || true

if ! command -v node >/dev/null 2>&1; then
  printf '✓ no pending decisions\n'
  exit 0
fi

width="${COLUMNS:-100}"
[ "$width" -ge 24 ] 2>/dev/null || width=100

# One row per needs-decision thread: "[slug] <first line / ~120 chars of statusText>".
# The scan + frontmatter parse lives in node (reuses the same logic as the rich
# view) so the shell stays a thin presenter.
node -e '
  const fs=require("fs"), path=require("path");
  const frayDir=path.join(process.argv[1],".fray");
  const KEYS=["statusText","status_text"];
  function fmOf(t){
    const L=t.split("\n"); if(L[0]!=="---")return null; const fm={};
    for(let i=1;i<L.length;i++){ if(L[i]==="---")return fm;
      const m=L[i].match(/^([\w-]+):\s*(.*)$/); if(m)fm[m[1]]=m[2]; }
    return null;
  }
  function unq(r){ if(r===undefined)return ""; let v=r.trim();
    const m=v.match(/^"((?:[^"\\]|\\.)*)"$/); if(m)v=m[1].replace(/\\(.)/g,"$1"); return v; }
  let files=[]; try{ files=fs.readdirSync(frayDir).filter(f=>f.endsWith(".md")).sort(); }catch{}
  for(const f of files){
    let t; try{ t=fs.readFileSync(path.join(frayDir,f),"utf8"); }catch{ continue; }
    const fm=fmOf(t); if(!fm||fm.status!=="needs-decision")continue;
    const raw=KEYS.map(k=>fm[k]).find(v=>v!==undefined);
    let s=unq(raw).split("\n")[0]; if(s.length>120)s=s.slice(0,119)+"…";
    process.stdout.write("["+f.replace(/\.md$/,"")+"] "+s+"\n");
  }
' "$dir" 2>/dev/null >/tmp/.fray-decisions.$$  || true

decisions="$(cat /tmp/.fray-decisions.$$ 2>/dev/null || true)"
rm -f /tmp/.fray-decisions.$$ 2>/dev/null || true

if [ -z "$decisions" ]; then
  printf '✓ no pending decisions\n'
  exit 0
fi

n=$(printf '%s\n' "$decisions" | grep -c .)

printf '⚖ %s decision(s) awaiting you:\n' "$n"

cap=10
printf '%s\n' "$decisions" | awk -v w="$width" -v cap="$cap" -v total="$n" '
  NR <= cap {
    line = " • " $0
    if (length(line) > w) line = substr(line, 1, w - 1) "…"
    print line
  }
  END {
    if (total > cap) printf " …(+%d more)\n", total - cap
  }
'
