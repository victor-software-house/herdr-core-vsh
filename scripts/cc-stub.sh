#!/bin/sh
# Stands in for the C compiler and archiver when only type-checking another
# target (clippy, check): leaves an empty file at each output. Nothing links.
#   CC_x86_64_pc_windows_msvc=$PWD/scripts/cc-stub.sh AR_x86_64_pc_windows_msvc=$PWD/scripts/cc-stub.sh \
#     cargo clippy --target x86_64-pc-windows-msvc
prev=
for arg in "$@"; do
    [ "$prev" = -o ] && : >"$arg"
    case $arg in
        -Fo?*) : >"${arg#-Fo}" ;;
        -out:?*) : >"${arg#-out:}" ;;
    esac
    prev=$arg
done
