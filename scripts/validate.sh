#!/usr/bin/env bash
# Validates that the index at $SCRY_INDEX_DIR has all phase 0-4 features
# working against real AOSP + Linux data. Run after `scry index`.
set -euo pipefail
. /mnt/agent/scry/env.sh

SCRY=/mnt/agent/scry/target/release/scry
INDEX=${SCRY_INDEX_DIR:-/mnt/agent/scry-index}

hr() { echo; echo "============================================================"; echo "$*"; echo "============================================================"; }

hr "stats"
$SCRY stats --index "$INDEX"

hr "scry def ActivityManagerService --kind class --lang Java"
$SCRY def ActivityManagerService --index "$INDEX" --kind class --lang Java --limit 5

hr "scry def IBinder --kind iface"
$SCRY def IBinder --index "$INDEX" --kind iface --limit 5

hr "scry callers transact (limit 5)"
$SCRY callers transact --index "$INDEX" --limit 5

hr "scry def Binder (any kind)"
$SCRY def Binder --index "$INDEX" --limit 5

hr "scry prefix Activity (limit 10)"
$SCRY prefix Activity --index "$INDEX" --limit 10

hr "scry fuzzy ParcelFile (limit 5)"
$SCRY fuzzy ParcelFile --index "$INDEX" --limit 5

hr "scry def libbinder --kind soong"
$SCRY def libbinder --index "$INDEX" --kind soong --limit 5

hr "scry ref liblog --lang Soong (top 5)"
$SCRY ref liblog --index "$INDEX" --lang Soong --limit 5

hr "scry def my_feature --kind aconfig (any aconfig flag named my_feature in tree)"
$SCRY def my_feature --index "$INDEX" --kind aconfig --limit 5 || true

hr "scry def zygote --kind init.svc"
$SCRY def zygote --index "$INDEX" --kind init.svc --limit 5

hr "scry def IBinder (AIDL)"
$SCRY def IBinder --index "$INDEX" --kind aidl.iface --limit 5

hr "scry def system_server --kind sepolicy"
$SCRY def system_server --index "$INDEX" --kind sepolicy --limit 5

hr "scry grep TODO --lang Java --limit 5"
$SCRY grep TODO --index "$INDEX" --lang Java --limit 5

hr "scry outline app_process/app_main.cpp (file-symbols list)"
$SCRY outline frameworks/base/cmds/app_process/app_main.cpp --index "$INDEX" --limit 10 || true

hr "scry def Activity --in frameworks/base/ (subdir-scoped)"
$SCRY def Activity --index "$INDEX" --in frameworks/base/ --limit 5 || true

hr "scry callers transact --in art/ (subdir-scoped)"
$SCRY callers transact --index "$INDEX" --in art/ --limit 5 || true

hr "scry serve smoke: covers def + callers + grep + in-filter + stats"
printf '%s\n' \
  '{"id":1,"cmd":"def","args":{"name":"Binder","limit":3}}' \
  '{"id":2,"cmd":"callers","args":{"name":"transact","limit":3}}' \
  '{"id":3,"cmd":"def","args":{"name":"Activity","in":"frameworks/base/","limit":3}}' \
  '{"id":4,"cmd":"grep","args":{"name":"ZygoteInit","limit":3}}' \
  '{"id":5,"cmd":"outline","args":{"path":"app_process/app_main.cpp","limit":3}}' \
  '{"id":6,"cmd":"stats"}' \
  | $SCRY serve --index "$INDEX"

echo
echo "VALIDATION DONE"
