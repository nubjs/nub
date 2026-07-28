#!/bin/bash
set -e
U=/tmp/undef.txt
OUT=/opt/xstub
sudo mkdir -p $OUT
TARGETS="[ arm64-macos, arm64e-macos, x86_64-macos ]"

emit() { # $1=outfile $2=install-name $3=regex(may be empty)
  local f="$1" iname="$2" rx="$3"
  {
    echo "--- !tapi-tbd"
    echo "tbd-version:     4"
    echo "targets:         $TARGETS"
    echo "install-name:    '$iname'"
    echo "current-version: 1.0.0"
    echo "compatibility-version: 1.0.0"
    if [ -n "$rx" ]; then
      local syms
      syms=$(grep -E "$rx" "$U" | sort -u | paste -sd, - | sed "s/,/, /g")
      if [ -n "$syms" ]; then
        echo "exports:"
        echo "  - targets:         $TARGETS"
        echo "    symbols:         [ $syms ]"
      fi
    fi
    echo "..."
  } | sudo tee "$f" > /dev/null
}

sudo mkdir -p $OUT/CoreFoundation.framework $OUT/Security.framework $OUT/SystemConfiguration.framework
emit $OUT/CoreFoundation.framework/CoreFoundation.tbd "/System/Library/Frameworks/CoreFoundation.framework/Versions/A/CoreFoundation" "^_(CF|kCF)"
emit $OUT/Security.framework/Security.tbd "/System/Library/Frameworks/Security.framework/Versions/A/Security" "^_(Sec|kSec)"
emit $OUT/SystemConfiguration.framework/SystemConfiguration.tbd "/System/Library/Frameworks/SystemConfiguration.framework/Versions/A/SystemConfiguration" "^_(SC|kSC)"
emit $OUT/libcompression.tbd "/usr/lib/libcompression.dylib" "^_compression_"
emit $OUT/libiconv.tbd "/usr/lib/libiconv.2.dylib" "^_(iconv|libiconv)"
