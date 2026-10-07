#!/bin/sh
# A stand-in for `alx check --json [--types] --overlays FILE|- FILE`: ignores
# the overlays file and reports no diagnostics; with --types, a table where
# `b` (bound at offset 0) is a `T` with one method `m`.
for a in "$@"; do
  if [ "$a" = "--types" ]; then
    printf '{"unit":"script","root":"/w","diagnostics":[],"types":[{"lo":0,"hi":1,"name":"b","type":"T","kind":"local"}],"members":{"T":[{"name":"m","kind":"method","sig":"-> Int"}]}}'
    exit 0
  fi
done
printf '{"unit":"script","root":"/w","diagnostics":[]}'
