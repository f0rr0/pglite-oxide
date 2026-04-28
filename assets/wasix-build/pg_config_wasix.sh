#!/usr/bin/env bash
set -euo pipefail

BUILD_DIR="${BUILD_DIR:-/work/assets/wasix-build/work/docker-pglite}"
PGSRC="${PGSRC:-/work/assets/checkouts/postgres-pglite}"
PREFIX="${PGLITE_WASIX_PREFIX:-$BUILD_DIR/install}"

case "${1:-}" in
  --pgxs)
    echo "$BUILD_DIR/src/makefiles/pgxs.mk"
    ;;
  --bindir)
    echo "$PREFIX/bin"
    ;;
  --sharedir)
    echo "$PREFIX/share"
    ;;
  --sysconfdir)
    echo "$PREFIX/etc"
    ;;
  --libdir)
    echo "$PREFIX/lib"
    ;;
  --pkglibdir)
    echo "$PREFIX/lib/postgresql"
    ;;
  --includedir | --pkgincludedir)
    echo "$PREFIX/include"
    ;;
  --mandir)
    echo "$PREFIX/share/man"
    ;;
  --docdir)
    echo "$PREFIX/share/doc"
    ;;
  --localedir)
    echo "$PREFIX/share/locale"
    ;;
  --version)
    echo "PostgreSQL 17.5-wasix-pglite"
    ;;
  --configure)
    echo "--host=wasm32-wasix --with-template=wasix-dl"
    ;;
  --cc)
    echo "wasixcc"
    ;;
  --cppflags)
    echo "-I$BUILD_DIR/src/include -I$PGSRC/src/include -I$PGSRC/src/include/port/wasix-dl"
    ;;
  --cflags)
    echo ""
    ;;
  --ldflags | --libs)
    echo ""
    ;;
  *)
    echo "unsupported pg_config_wasix.sh option: ${1:-<none>}" >&2
    exit 2
    ;;
esac
