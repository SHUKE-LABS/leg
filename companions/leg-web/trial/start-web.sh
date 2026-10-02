#!/bin/sh
set -eu

bundle_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
exec "${bundle_dir}/bin/leg-web" \
  --leg-bin "${bundle_dir}/bin/leg" \
  --supervisor-bin "${bundle_dir}/bin/leg-ui-supervisor" \
  "$@"
