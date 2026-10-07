#!/bin/sh
# A stand-in for `alx check --json [--types] ...` whose type table never
# knows a receiver: every member completion runs it again (the soak's
# EVERY mode, a real child process per edit).
printf '{"unit":"script","root":"/w","diagnostics":[],"types":[],"members":{}}'
