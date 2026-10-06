#!/bin/sh
# A stand-in for `alx check --json --overlays FILE|- FILE`: ignores the overlays file
# and and reports no diagnostics.
printf '{"unit":"script","root":"/w","diagnostics":[]}'
