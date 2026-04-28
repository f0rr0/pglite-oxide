#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$ROOT/../.." && pwd)"
DEFAULT_PGSRC="$ROOT/work/postgres-pglite-wasix-src"
if [ ! -d "$DEFAULT_PGSRC" ]; then
  DEFAULT_PGSRC="$REPO_ROOT/spikes/upstream/postgres-pglite"
fi
PGSRC="${PGSRC:-$DEFAULT_PGSRC}"
BUILD="${BUILD_DIR:-$ROOT/work/configure-smoke}"

WASIX_HOME="${WASIX_HOME:-/tmp/wasixcc-home/.wasixcc}"
export HOME="${WASIX_HOME%/.wasixcc}"
export PATH="$WASIX_HOME/bin:$PATH"

mkdir -p "$BUILD"

COMMON_CPPFLAGS="-I$PGSRC/src/include/port/wasix-dl"
COMMON_CFLAGS="-sWASM_EXCEPTIONS=yes -sPIC=yes -Wno-unused-command-line-argument"
COMMON_LDFLAGS="-sWASM_EXCEPTIONS=yes -sPIC=yes"
MAIN_LDFLAGS="-sMODULE_KIND=dynamic-main"
SIDE_MODULE_LDFLAGS="-Wl,-shared"

if [ "${PGLITE_MODE:-0}" = "1" ]; then
  mkdir -p "$ROOT/build/wasix-pglite"
  PGLITE_SHIM="$ROOT/build/wasix-pglite/pglite_wasix_bridge.o"

  wasixcc $COMMON_CFLAGS \
    -include stdbool.h \
    -include stdlib.h \
    -I"$PGSRC/src/include/port/wasix-dl" \
    -c "$ROOT/wasix_shim/pglite_wasix_bridge.c" \
    -o "$PGLITE_SHIM"

  PGLITE_CFLAGS="\
 -D__PGLITE__\
 -DPGLITE_WASIX_DL\
 -Dsystem=pgl_system -Dpopen=pgl_popen -Dpclose=pgl_pclose\
 -Dgeteuid=pgl_geteuid -Dgetuid=pgl_getuid -Dgetpwuid=pgl_getpwuid\
 -Dexit=pgl_exit\
 -Dmunmap=pgl_munmap\
 -Dfcntl=pgl_fcntl\
 -Datexit=pgl_atexit\
 -Dsetsockopt=pgl_setsockopt -Dgetsockopt=pgl_getsockopt -Dgetsockname=pgl_getsockname\
 -Drecv=pgl_recv -Dsend=pgl_send -Dconnect=pgl_connect\
 -Dpoll=pgl_poll\
 -Dshmget=pgl_shmget -Dshmat=pgl_shmat -Dshmdt=pgl_shmdt -Dshmctl=pgl_shmctl\
 -Wno-declaration-after-statement\
 -Wno-macro-redefined\
 -Wno-unused-function\
 -Wno-missing-prototypes\
 -Wno-incompatible-pointer-types"
  LDFLAGS_EXTRA=" $PGLITE_SHIM"
else
  mkdir -p "$ROOT/build/wasix-shim"
  GENERIC_SHIM="$ROOT/build/wasix-shim/pglite_wasix_shim.o"

  wasixcc $COMMON_CFLAGS \
    -I"$PGSRC/src/include/port/wasix-dl" \
    -c "$ROOT/wasix_shim/pglite_wasix_shim.c" \
    -o "$GENERIC_SHIM"

  PGLITE_CFLAGS=""
  LDFLAGS_EXTRA=" $GENERIC_SHIM"
fi

cd "$BUILD"

CC=wasixcc \
AR=wasixar \
RANLIB=wasixranlib \
NM=wasixnm \
CPPFLAGS="$COMMON_CPPFLAGS" \
CFLAGS="$COMMON_CFLAGS$PGLITE_CFLAGS" \
LDFLAGS="$COMMON_LDFLAGS" \
LDFLAGS_EX="$MAIN_LDFLAGS$LDFLAGS_EXTRA" \
LDFLAGS_SL="$SIDE_MODULE_LDFLAGS" \
"$PGSRC/configure" \
  --prefix=/ \
  --libdir=/lib \
  --datadir=/share/postgresql \
  --bindir=/bin \
  --host=wasm32-wasix \
  --with-template=wasix-dl \
  --without-readline \
  --without-icu \
  --without-zlib \
  --without-llvm \
  --disable-spinlocks \
  --disable-largefile \
  --without-pam \
  --with-openssl=no
