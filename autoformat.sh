#!/bin/sh
set -e
cargo fmt
ruff format -q -n protogen.py waypipe-c/test/*.py waypipe-c/protocols/*.py
clang-format -style=file --assume-filename=C -i waypipe-c/*.h waypipe-c/*.c waypipe-c/test/*.c waypipe-c/test/*.h
clang-format -style=llvm -i shaders/*.glsl
meson fmt -i meson.build waypipe-c/meson.build waypipe-c/protocols/meson.build waypipe-c/test/meson.build
