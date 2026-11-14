#!/usr/bin/env bash
# Check a BAF packet dump against Mojang's own 26.3 client code.
#
#   tools/vanilla-oracle/run.sh <dump.bin> [--all]
#   tools/vanilla-oracle/run.sh --selftest [out.bin]
#
# Needs Java 25 (JAVA_HOME or java on PATH), curl and about 1 GB of RAM.
# Downloads the official 26.3 client.jar and server.jar once (sha1-checked),
# takes the libraries out of the server bundler, compiles ClickOracle.java and
# runs it. Cache: $VANILLA_ORACLE_CACHE, default ~/.cache/baf-vanilla-oracle.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CACHE="${VANILLA_ORACLE_CACHE:-$HOME/.cache/baf-vanilla-oracle}"
XMX="${VANILLA_ORACLE_XMX:-1g}"

# From https://piston-meta.mojang.com/v1/packages/9f67e10ed013deeaad20ac3c800652765d493408/26.3.json
CLIENT_SHA1=e877b6a07acd633fb3bb475002175cec036e7b87
SERVER_SHA1=33680f5f2ac32864d6d7cf5e56a705fdb3e05f4c

if [[ -n "${JAVA_HOME:-}" ]]; then
  JAVA="$JAVA_HOME/bin/java"
  JAVAC="$JAVA_HOME/bin/javac"
  JAR="$JAVA_HOME/bin/jar"
else
  JAVA=java
  JAVAC=javac
  JAR=jar
fi
if ! "$JAVA" -version 2>&1 | grep -qE 'version "(2[5-9]|[3-9][0-9])'; then
  echo "Java 25 or newer is required (Minecraft 26.3 targets Java 25). Set JAVA_HOME." >&2
  "$JAVA" -version >&2 || true
  exit 2
fi

mkdir -p "$CACHE"

fetch() {
  local name=$1 sha1=$2 url="https://piston-data.mojang.com/v1/objects/$2/$1"
  local dest="$CACHE/$name"
  if [[ -f "$dest" ]] && echo "$sha1  $dest" | sha1sum -c --status; then
    return
  fi
  echo "downloading $name" >&2
  curl -fsSL -o "$dest.part" "$url"
  echo "$sha1  $dest.part" | sha1sum -c --status || { echo "sha1 mismatch for $name" >&2; rm -f "$dest.part"; exit 3; }
  mv "$dest.part" "$dest"
}

fetch client.jar "$CLIENT_SHA1"
fetch server.jar "$SERVER_SHA1"

LIBS="$CACHE/libs-$SERVER_SHA1"
if [[ ! -d "$LIBS/META-INF/libraries" ]]; then
  rm -rf "$LIBS.part"
  mkdir -p "$LIBS.part"
  (cd "$LIBS.part" && "$JAR" xf "$CACHE/server.jar" META-INF/libraries)
  mv "$LIBS.part" "$LIBS"
fi

CP="$CACHE/client.jar"
while IFS= read -r lib; do
  CP="$CP:$lib"
done < <(find "$LIBS/META-INF/libraries" -name '*.jar' | sort)

CLASSES="$CACHE/classes"
STAMP="$CLASSES/.source-sha1"
SRC_SHA1=$(sha1sum "$HERE/ClickOracle.java" | cut -d' ' -f1)
if [[ ! -f "$STAMP" || "$(cat "$STAMP")" != "$SRC_SHA1" ]]; then
  rm -rf "$CLASSES"
  mkdir -p "$CLASSES"
  "$JAVAC" -J-Xmx512m -nowarn -encoding UTF-8 -d "$CLASSES" -cp "$CP" "$HERE/ClickOracle.java"
  echo "$SRC_SHA1" > "$STAMP"
fi

exec "$JAVA" -Xmx"$XMX" --enable-native-access=ALL-UNNAMED -cp "$CLASSES:$CP" ClickOracle "$@"
